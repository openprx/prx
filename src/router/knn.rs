use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
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
            embedding_b64: BASE64_STANDARD.encode(crate::memory::vector::vec_to_bytes(
                &record.embedding,
            )),
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
    pub async fn new(memory: Arc<dyn Memory>) -> Result<Self> {
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

        Some((winner.to_string(), (winner_weight / total_weight).clamp(0.0, 1.0)))
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
