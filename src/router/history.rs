use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::time::timeout;
use uuid::Uuid;

use crate::memory::embeddings::EmbeddingProvider;

use super::knn::{KnnStore, MIN_RECORDS_FOR_KNN, QueryRecord, weighted_model_score};

const DEFAULT_KNN_TIMEOUT: Duration = Duration::from_millis(100);
#[cfg(test)]
const DEFAULT_KNN_K: usize = 7;

pub struct RouterHistory {
    store: KnnStore,
    embedder: Arc<dyn EmbeddingProvider>,
    knn_k: usize,
    knn_min_records: usize,
    query_timeout: Duration,
}

impl RouterHistory {
    pub fn new(
        store: KnnStore,
        embedder: Arc<dyn EmbeddingProvider>,
        knn_k: usize,
        knn_min_records: usize,
    ) -> Self {
        Self {
            store,
            embedder,
            knn_k: knn_k.max(1),
            knn_min_records: knn_min_records.max(MIN_RECORDS_FOR_KNN),
            query_timeout: DEFAULT_KNN_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_timeout(mut self, query_timeout: Duration) -> Self {
        self.query_timeout = query_timeout;
        self
    }

    pub async fn record_query(
        &self,
        message: &str,
        chosen_model: &str,
        success: bool,
    ) -> Result<()> {
        let embedding = self.embedder.embed_one(message).await?;
        self.store
            .insert(QueryRecord {
                query_id: Uuid::new_v4().to_string(),
                embedding,
                chosen_model_id: chosen_model.to_string(),
                success,
                timestamp: chrono::Utc::now().timestamp(),
            })
            .await
    }

    pub async fn similarity_score(&self, message: &str, model_id: &str) -> f32 {
        self.similarity_scores(message)
            .await
            .get(model_id)
            .copied()
            .unwrap_or(0.0)
    }

    pub async fn similarity_scores(&self, message: &str) -> HashMap<String, f32> {
        let record_count = match self.store.count().await {
            Ok(count) => count,
            Err(err) => {
                tracing::warn!("Router KNN count failed: {err}");
                return HashMap::new();
            }
        };
        if record_count < self.knn_min_records {
            return HashMap::new();
        }

        let lookup = async {
            let embedding = self.embedder.embed_one(message).await?;
            self.store.search(&embedding, self.knn_k).await
        };

        let neighbors = match timeout(self.query_timeout, lookup).await {
            Ok(Ok(neighbors)) => neighbors,
            Ok(Err(err)) => {
                tracing::warn!("Router KNN lookup failed: {err}");
                return HashMap::new();
            }
            Err(_) => {
                tracing::warn!("Router KNN lookup timed out after {:?}", self.query_timeout);
                return HashMap::new();
            }
        };
        if neighbors.is_empty() {
            return HashMap::new();
        }

        let mut scores = HashMap::new();
        for (model_id, _) in &neighbors {
            scores
                .entry(model_id.clone())
                .or_insert_with(|| weighted_model_score(&neighbors, model_id));
        }

        scores
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::embeddings::EmbeddingProvider;
    use crate::memory::none::NoneMemory;
    use async_trait::async_trait;

    /// Mock embedder that returns a fixed unit vector for any input.
    struct FixedEmbedder;

    #[async_trait]
    impl EmbeddingProvider for FixedEmbedder {
        fn name(&self) -> &str {
            "fixed"
        }
        fn dimensions(&self) -> usize {
            3
        }
        async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0]).collect())
        }
    }

    fn make_history() -> RouterHistory {
        let memory = Arc::new(NoneMemory);
        let store = KnnStore::new(memory).unwrap();
        RouterHistory::new(
            store,
            Arc::new(FixedEmbedder),
            DEFAULT_KNN_K,
            MIN_RECORDS_FOR_KNN,
        )
    }

    #[test]
    fn new_clamps_k_to_at_least_one() {
        let memory = Arc::new(NoneMemory);
        let store = KnnStore::new(memory).unwrap();
        let history = RouterHistory::new(store, Arc::new(FixedEmbedder), 0, 0);
        assert_eq!(history.knn_k, 1);
    }

    #[test]
    fn new_clamps_min_records() {
        let memory = Arc::new(NoneMemory);
        let store = KnnStore::new(memory).unwrap();
        let history = RouterHistory::new(store, Arc::new(FixedEmbedder), 5, 0);
        assert!(history.knn_min_records >= MIN_RECORDS_FOR_KNN);
    }

    #[tokio::test]
    async fn record_query_does_not_panic() {
        let history = make_history();
        let result = history.record_query("test message", "gpt-4", true).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn similarity_score_empty_store_returns_zero() {
        let history = make_history();
        let score = history.similarity_score("hello", "gpt-4").await;
        assert!((score - 0.0).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn similarity_scores_empty_store_returns_empty_map() {
        let history = make_history();
        let scores = history.similarity_scores("hello").await;
        assert!(scores.is_empty());
    }

    #[tokio::test]
    async fn with_timeout_is_applied() {
        let history = make_history().with_timeout(Duration::from_secs(5));
        assert_eq!(history.query_timeout, Duration::from_secs(5));
    }
}
