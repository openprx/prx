use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

use crate::memory::{Memory, MemoryCategory};
use crate::self_system::SELF_SYSTEM_SESSION_ID;

pub const MIN_RECORDS_FOR_KNN: usize = 10;
const ROUTER_HISTORY_CATEGORY: &str = "router_history";
const ROUTER_HISTORY_KEY_PREFIX: &str = "router/history/";
const MIN_DISTANCE_EPSILON: f32 = 1.0e-6;

#[derive(Debug, Clone)]
pub struct QueryRecord {
    pub query_id: String,
    pub embedding: Vec<f32>,
    pub chosen_model_id: String,
    pub success: bool,
    pub timestamp: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredQueryRecord {
    query_id: String,
    embedding_b64: String,
    chosen_model_id: String,
    success: bool,
    timestamp: i64,
}

impl StoredQueryRecord {
    fn from_record(record: QueryRecord) -> Self {
        Self {
            query_id: record.query_id,
            embedding_b64: BASE64_STANDARD
                .encode(crate::memory::vector::vec_to_bytes(&record.embedding)),
            chosen_model_id: record.chosen_model_id,
            success: record.success,
            timestamp: record.timestamp,
        }
    }

    fn into_record(self) -> Option<QueryRecord> {
        let bytes = BASE64_STANDARD.decode(self.embedding_b64).ok()?;
        Some(QueryRecord {
            query_id: self.query_id,
            embedding: crate::memory::vector::bytes_to_vec(&bytes),
            chosen_model_id: self.chosen_model_id,
            success: self.success,
            timestamp: self.timestamp,
        })
    }
}

pub struct KnnStore {
    memory: Arc<dyn Memory>,
}

impl KnnStore {
    pub fn new(memory: Arc<dyn Memory>) -> Result<Self> {
        Ok(Self { memory })
    }

    pub async fn insert(&self, record: QueryRecord) -> Result<()> {
        let stored = StoredQueryRecord::from_record(record);
        let payload = serde_json::to_string(&stored)?;
        self.memory
            .store(
                &format!("{ROUTER_HISTORY_KEY_PREFIX}{}", stored.query_id),
                &payload,
                MemoryCategory::Custom(ROUTER_HISTORY_CATEGORY.to_string()),
                Some(SELF_SYSTEM_SESSION_ID),
            )
            .await
    }

    pub async fn search(&self, embedding: &[f32], k: usize) -> Result<Vec<(String, f32)>> {
        let entries = self
            .memory
            .list(
                Some(&MemoryCategory::Custom(ROUTER_HISTORY_CATEGORY.to_string())),
                Some(SELF_SYSTEM_SESSION_ID),
            )
            .await?;

        let mut scored = Vec::new();
        for entry in entries {
            if !entry.key.starts_with(ROUTER_HISTORY_KEY_PREFIX) {
                continue;
            }

            let Some(record) = serde_json::from_str::<StoredQueryRecord>(&entry.content)
                .ok()
                .and_then(StoredQueryRecord::into_record)
            else {
                continue;
            };

            if !record.success {
                continue;
            }

            let similarity = crate::memory::vector::cosine_similarity(embedding, &record.embedding);
            if similarity <= 0.0 {
                continue;
            }

            scored.push((record.chosen_model_id, 1.0 - similarity));
        }

        scored.sort_by(|left, right| left.1.partial_cmp(&right.1).unwrap_or(Ordering::Equal));
        scored.truncate(k);
        Ok(scored)
    }

    pub fn majority_vote(&self, neighbors: &[(String, f32)]) -> Option<(String, f32)> {
        let mut weights_by_model: HashMap<&str, f32> = HashMap::new();
        let mut total_weight = 0.0_f32;

        for (model_id, distance) in neighbors {
            let weight = 1.0 / distance.max(MIN_DISTANCE_EPSILON);
            total_weight += weight;
            *weights_by_model.entry(model_id.as_str()).or_default() += weight;
        }

        let (winner, winner_weight) = weights_by_model
            .into_iter()
            .max_by(|left, right| left.1.partial_cmp(&right.1).unwrap_or(Ordering::Equal))?;

        if total_weight <= 0.0 {
            return None;
        }

        Some((
            winner.to_string(),
            (winner_weight / total_weight).clamp(0.0, 1.0),
        ))
    }

    pub async fn count(&self) -> Result<usize> {
        let entries = self
            .memory
            .list(
                Some(&MemoryCategory::Custom(ROUTER_HISTORY_CATEGORY.to_string())),
                Some(SELF_SYSTEM_SESSION_ID),
            )
            .await?;

        Ok(entries
            .iter()
            .filter(|entry| entry.key.starts_with(ROUTER_HISTORY_KEY_PREFIX))
            .count())
    }
}

pub(crate) fn weighted_model_score(neighbors: &[(String, f32)], model_id: &str) -> f32 {
    let mut total_weight = 0.0_f32;
    let mut model_weight = 0.0_f32;

    for (neighbor_model, distance) in neighbors {
        let weight = 1.0 / distance.max(MIN_DISTANCE_EPSILON);
        total_weight += weight;
        if neighbor_model == model_id {
            model_weight += weight;
        }
    }

    if total_weight <= 0.0 {
        return 0.0;
    }

    (model_weight / total_weight).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::none::NoneMemory;

    // ── weighted_model_score ────────────────────────────────────

    #[test]
    fn weighted_score_single_model() {
        let neighbors = vec![("gpt-4".to_string(), 0.1)];
        let score = weighted_model_score(&neighbors, "gpt-4");
        assert!((score - 1.0).abs() < 0.01);
    }

    #[test]
    fn weighted_score_absent_model() {
        let neighbors = vec![("gpt-4".to_string(), 0.1)];
        let score = weighted_model_score(&neighbors, "claude-3");
        assert!((score - 0.0).abs() < 0.01);
    }

    #[test]
    fn weighted_score_two_models_equal_distance() {
        let neighbors = vec![("gpt-4".to_string(), 0.5), ("claude-3".to_string(), 0.5)];
        let gpt_score = weighted_model_score(&neighbors, "gpt-4");
        let claude_score = weighted_model_score(&neighbors, "claude-3");
        assert!((gpt_score - 0.5).abs() < 0.01);
        assert!((claude_score - 0.5).abs() < 0.01);
    }

    #[test]
    fn weighted_score_closer_model_wins() {
        let neighbors = vec![
            ("gpt-4".to_string(), 0.1),    // close → high weight
            ("claude-3".to_string(), 1.0), // far → low weight
        ];
        let gpt_score = weighted_model_score(&neighbors, "gpt-4");
        assert!(
            gpt_score > 0.5,
            "closer model should have higher score: {gpt_score}"
        );
    }

    #[test]
    fn weighted_score_empty_neighbors() {
        let score = weighted_model_score(&[], "gpt-4");
        assert!((score - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn weighted_score_zero_distance_uses_epsilon() {
        let neighbors = vec![("gpt-4".to_string(), 0.0)];
        let score = weighted_model_score(&neighbors, "gpt-4");
        // distance clamped to MIN_DISTANCE_EPSILON → still valid
        assert!((score - 1.0).abs() < 0.01);
    }

    // ── KnnStore::majority_vote ─────────────────────────────────

    #[test]
    fn majority_vote_single_winner() {
        let store = KnnStore::new(Arc::new(NoneMemory)).unwrap();
        let neighbors = vec![
            ("gpt-4".to_string(), 0.1),
            ("gpt-4".to_string(), 0.2),
            ("claude-3".to_string(), 0.5),
        ];
        let (winner, confidence) = store.majority_vote(&neighbors).unwrap();
        assert_eq!(winner, "gpt-4");
        assert!(confidence > 0.5);
    }

    #[test]
    fn majority_vote_empty() {
        let store = KnnStore::new(Arc::new(NoneMemory)).unwrap();
        assert!(store.majority_vote(&[]).is_none());
    }

    #[test]
    fn majority_vote_tie_resolved_by_distance() {
        let store = KnnStore::new(Arc::new(NoneMemory)).unwrap();
        let neighbors = vec![
            ("gpt-4".to_string(), 0.1),    // 1/0.1 = 10
            ("claude-3".to_string(), 0.5), // 1/0.5 = 2
        ];
        let (winner, _) = store.majority_vote(&neighbors).unwrap();
        assert_eq!(winner, "gpt-4", "closer model wins the tie");
    }

    // ── KnnStore with NoneMemory ────────────────────────────────

    #[tokio::test]
    async fn store_count_empty() {
        let store = KnnStore::new(Arc::new(NoneMemory)).unwrap();
        assert_eq!(store.count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn store_search_empty_returns_empty() {
        let store = KnnStore::new(Arc::new(NoneMemory)).unwrap();
        let results = store.search(&[1.0, 0.0, 0.0], 5).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn store_insert_does_not_panic() {
        let store = KnnStore::new(Arc::new(NoneMemory)).unwrap();
        let result = store
            .insert(QueryRecord {
                query_id: "q1".to_string(),
                embedding: vec![1.0, 0.0],
                chosen_model_id: "gpt-4".to_string(),
                success: true,
                timestamp: 1000,
            })
            .await;
        assert!(result.is_ok());
    }

    // ── StoredQueryRecord roundtrip ─────────────────────────────

    #[test]
    fn stored_record_roundtrip() {
        let original = QueryRecord {
            query_id: "q42".to_string(),
            embedding: vec![0.1, 0.2, 0.3],
            chosen_model_id: "claude-3".to_string(),
            success: true,
            timestamp: 12345,
        };
        let stored = StoredQueryRecord::from_record(original.clone());
        let restored = stored.into_record().expect("test: roundtrip");
        assert_eq!(restored.query_id, "q42");
        assert_eq!(restored.chosen_model_id, "claude-3");
        assert_eq!(restored.embedding.len(), 3);
        assert!((restored.embedding[0] - 0.1).abs() < 0.001);
    }
}
