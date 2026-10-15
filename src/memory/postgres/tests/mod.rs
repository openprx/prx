//! Tests for the PostgreSQL memory backend.

mod basics;
mod documents;
mod event_reads;
mod memory_owner_isolation;
mod owner_isolation;
mod pool_conformance;
mod rls;
mod upsert;

use crate::memory::embeddings;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(super) struct CountingEmbedding {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl embeddings::EmbeddingProvider for CountingEmbedding {
    fn name(&self) -> &str {
        "counting"
    }

    fn dimensions(&self) -> usize {
        3
    }

    fn model(&self) -> &str {
        "counting-v1"
    }

    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3]).collect())
    }
}
