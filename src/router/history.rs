use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::time::timeout;
use uuid::Uuid;

use crate::memory::embeddings::{EmbeddingProvider, LocalHashEmbedding};

use super::knn::{KnnStore, MIN_RECORDS_FOR_KNN, QueryRecord, weighted_model_score};

const DEFAULT_KNN_TIMEOUT: Duration = Duration::from_millis(100);
#[cfg(test)]
const DEFAULT_KNN_K: usize = 7;

pub struct RouterHistory {
    store: KnnStore,
    embedder: Arc<dyn EmbeddingProvider>,
    local_embedder: Option<Arc<LocalHashEmbedding>>,
    knn_k: usize,
    knn_min_records: usize,
    query_timeout: Duration,
}

impl RouterHistory {
    pub fn new(store: KnnStore, embedder: Arc<dyn EmbeddingProvider>, knn_k: usize, knn_min_records: usize) -> Self {
        Self {
            store,
            embedder,
            local_embedder: None,
            knn_k: knn_k.max(1),
            knn_min_records: knn_min_records.max(MIN_RECORDS_FOR_KNN),
            query_timeout: DEFAULT_KNN_TIMEOUT,
        }
    }

    /// Retain the concrete instance supplied by the caller. A dynamic
    /// provider's name is never accepted as proof of local, uncharged work.
    pub fn new_local_hash(
        store: KnnStore,
        embedder: Arc<LocalHashEmbedding>,
        knn_k: usize,
        knn_min_records: usize,
    ) -> Result<Self> {
        anyhow::ensure!(
            (1..=2048).contains(&embedder.dimensions())
                && !embedder.model().is_empty()
                && embedder.model().len() <= 512,
            "Router local embedding descriptor exceeds capacity"
        );
        anyhow::ensure!(
            knn_k <= 1024 && knn_min_records <= 1024,
            "Router history policy exceeds capacity"
        );
        Ok(Self {
            store,
            embedder: embedder.clone(),
            local_embedder: Some(embedder),
            knn_k: knn_k.max(1),
            knn_min_records: knn_min_records.max(MIN_RECORDS_FOR_KNN),
            query_timeout: DEFAULT_KNN_TIMEOUT,
        })
    }

    pub(crate) fn query_deadline(&self, turn_deadline: std::time::Instant) -> Result<std::time::Instant> {
        let local = std::time::Instant::now()
            .checked_add(self.query_timeout)
            .ok_or_else(|| anyhow::anyhow!("Router history deadline overflow"))?;
        Ok(local.min(turn_deadline))
    }

    pub(crate) fn policy_descriptor(&self) -> Result<(&str, usize, usize, usize, u128)> {
        let embedder = self.admitted_embedder()?;
        Ok((
            embedder.model(),
            embedder.dimensions(),
            self.knn_k,
            self.knn_min_records,
            self.query_timeout.as_nanos(),
        ))
    }

    pub(crate) const fn local_embedder_if_present(&self) -> Option<&Arc<LocalHashEmbedding>> {
        self.local_embedder.as_ref()
    }

    pub(crate) fn admitted_embedder(&self) -> Result<&dyn EmbeddingProvider> {
        if let Some(local) = self.local_embedder.as_ref() {
            return Ok(local.as_ref());
        }
        anyhow::ensure!(
            self.embedder.supports_attempt_accounting(),
            "Router paid embedder lacks physical accounting"
        );
        anyhow::ensure!(
            (1..=2048).contains(&self.embedder.dimensions())
                && !self.embedder.model().is_empty()
                && self.embedder.model().len() <= 512
                && self.knn_k <= 1024
                && self.knn_min_records <= 1024,
            "Router paid embedding descriptor exceeds capacity"
        );
        Ok(self.embedder.as_ref())
    }

    /// Rank a complete authorized history domain, keeping provider/model pairs
    /// distinct. Failed records still count toward the original KNN threshold.
    pub(crate) async fn scoped_similarity(
        &self,
        read: &crate::memory::RouterHistoryRead,
        domain: &crate::memory::RouterHistoryDomain,
        vector: &[f32],
        deadline: std::time::Instant,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<HashMap<(String, String), f32>> {
        read.validate(domain)?;
        anyhow::ensure!(
            vector.len() == usize::from(domain.dimensions) && vector.iter().all(|value| value.is_finite()),
            "invalid Router query embedding"
        );
        anyhow::ensure!(
            !cancellation.is_cancelled() && std::time::Instant::now() < deadline,
            "Router history ranking cancelled or expired"
        );
        if read.rows.len() < self.knn_min_records {
            return Ok(HashMap::new());
        }
        let mut neighbors = Vec::new();
        for row in &read.rows {
            tokio::task::yield_now().await;
            anyhow::ensure!(
                !cancellation.is_cancelled() && std::time::Instant::now() < deadline,
                "Router history ranking cancelled or expired"
            );
            if !row.success {
                continue;
            }
            let similarity = crate::memory::vector::cosine_similarity(vector, &row.embedding);
            anyhow::ensure!(similarity.is_finite(), "invalid Router history cosine");
            if similarity > 0.0 {
                neighbors.push((row, 1.0 - similarity));
            }
        }
        neighbors.sort_by(|left, right| left.1.total_cmp(&right.1));
        neighbors.truncate(self.knn_k);
        let mut scores = HashMap::new();
        let mut total = 0.0_f32;
        for (row, distance) in neighbors {
            let weight = 1.0 / distance.max(1.0e-6);
            total += weight;
            *scores.entry((row.provider.clone(), row.model.clone())).or_insert(0.0) += weight;
        }
        if total > 0.0 {
            for value in scores.values_mut() {
                *value = (*value / total).clamp(0.0, 1.0);
            }
        }
        anyhow::ensure!(
            !cancellation.is_cancelled() && std::time::Instant::now() < deadline,
            "Router history ranking cancelled or expired"
        );
        Ok(scores)
    }

    #[cfg(test)]
    pub(crate) const fn with_timeout(mut self, query_timeout: Duration) -> Self {
        self.query_timeout = query_timeout;
        self
    }

    pub async fn record_query(&self, message: &str, chosen_model: &str, success: bool) -> Result<()> {
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
            Err(_) => {
                tracing::warn!("Router KNN count failed");
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
            Ok(Err(_)) => {
                // Backend errors may echo the query or credentials.
                tracing::warn!("Router KNN lookup failed");
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
        RouterHistory::new(store, Arc::new(FixedEmbedder), DEFAULT_KNN_K, MIN_RECORDS_FOR_KNN)
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
