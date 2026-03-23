use crate::memory::{LifecycleState, MemoryCategory, MemoryEntry};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::HashSet;
use uuid::Uuid;

/// Compression trigger limits.
#[derive(Debug, Clone, Copy)]
pub struct CompressionLimits {
    pub max_entries: usize,
    pub max_tokens: usize,
}

/// Similarity detector extension point for memory redundancy detection.
#[async_trait]
pub trait SimilarityDetector: Send + Sync {
    async fn detect_redundant_ids(&self, _entries: &[MemoryEntry]) -> Result<Vec<String>>;
}

/// Embedding-first redundancy detector with conservative fallback similarity.
pub struct EmbeddingSimilarityDetector {
    threshold: f64,
}

impl EmbeddingSimilarityDetector {
    pub fn new(threshold: f64) -> Self {
        Self {
            threshold: threshold.clamp(0.8, 0.99),
        }
    }
}

impl Default for EmbeddingSimilarityDetector {
    fn default() -> Self {
        Self::new(0.92)
    }
}

#[async_trait]
impl SimilarityDetector for EmbeddingSimilarityDetector {
    async fn detect_redundant_ids(&self, entries: &[MemoryEntry]) -> Result<Vec<String>> {
        let mut redundant = HashSet::new();
        for i in 0..entries.len() {
            if redundant.contains(&entries[i].id) {
                continue;
            }
            for j in (i + 1)..entries.len() {
                if redundant.contains(&entries[j].id) {
                    continue;
                }

                let similarity = entry_similarity(&entries[i], &entries[j]);
                if similarity <= self.threshold {
                    continue;
                }

                if should_keep_left(&entries[i], &entries[j]) {
                    redundant.insert(entries[j].id.clone());
                } else {
                    redundant.insert(entries[i].id.clone());
                }
            }
        }

        let mut ids = redundant.into_iter().collect::<Vec<_>>();
        ids.sort();
        Ok(ids)
    }
}

/// Fidelity verification result.
#[derive(Debug, Clone, PartialEq)]
pub struct FidelityReport {
    pub missing_facts: Vec<String>,
    pub loss_rate: f64,
}

/// Compression result with accept/reject decision.
#[derive(Debug, Clone)]
pub struct CompressionResult {
    pub accepted: bool,
    pub reason: String,
    pub report: FidelityReport,
    pub entry: Option<MemoryEntry>,
}

/// Memory compressor with trigger checks and fidelity guardrails.
pub struct MemoryCompressor<D: SimilarityDetector> {
    detector: D,
}

impl<D: SimilarityDetector> MemoryCompressor<D> {
    pub fn new(detector: D) -> Self {
        Self { detector }
    }

    /// Trigger compression once either entry count or token budget exceeds limits.
    pub fn should_trigger(&self, entries: &[MemoryEntry], limits: CompressionLimits) -> bool {
        if entries.len() > limits.max_entries {
            return true;
        }
        let tokens = entries
            .iter()
            .map(|entry| estimate_tokens(&entry.content))
            .sum::<usize>();
        tokens > limits.max_tokens
    }

    /// Redundancy detector hook.
    pub async fn detect_redundancy(&self, entries: &[MemoryEntry]) -> Result<Vec<String>> {
        self.detector.detect_redundant_ids(entries).await
    }

    /// Verify critical fact retention. Reject when loss rate exceeds 10%.
    pub fn verify_fidelity(&self, originals: &[MemoryEntry], compressed_content: &str) -> FidelityReport {
        let original_facts = extract_key_facts(originals);
        if original_facts.is_empty() {
            return FidelityReport {
                missing_facts: Vec::new(),
                loss_rate: 0.0,
            };
        }

        let compressed_lc = compressed_content.to_ascii_lowercase();
        let mut missing = Vec::new();
        for fact in &original_facts {
            if !compressed_lc.contains(fact) {
                missing.push(fact.clone());
            }
        }

        let loss_rate = missing.len() as f64 / original_facts.len() as f64;
        FidelityReport {
            missing_facts: missing,
            loss_rate,
        }
    }

    /// Build compressed entry with audit chain via `compressed_from`.
    pub fn build_compressed_entry(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        originals: &[MemoryEntry],
    ) -> MemoryEntry {
        MemoryEntry {
            id: format!("cmp-{}", Uuid::now_v7()),
            key: key.to_string(),
            content: content.to_string(),
            category,
            timestamp: Utc::now().to_rfc3339(),
            session_id: None,
            score: None,
            tags: Some(vec!["compressed".to_string()]),
            access_count: Some(0),
            useful_count: Some(0),
            source: Some("memory_compressor".to_string()),
            source_confidence: Some(0.8),
            verification_status: None,
            lifecycle_state: Some(LifecycleState::Active),
            compressed_from: Some(originals.iter().map(|entry| entry.id.clone()).collect()),
        }
    }

    /// Validate compression output and enforce fidelity threshold.
    pub fn finalize_compression(&self, originals: &[MemoryEntry], candidate: MemoryEntry) -> Result<CompressionResult> {
        let report = self.verify_fidelity(originals, &candidate.content);
        if report.loss_rate > 0.10 {
            return Ok(CompressionResult {
                accepted: false,
                reason: format!("fidelity loss {:.2}% exceeds 10%", report.loss_rate * 100.0),
                report,
                entry: None,
            });
        }

        Ok(CompressionResult {
            accepted: true,
            reason: "compression accepted".to_string(),
            report,
            entry: Some(candidate),
        })
    }
}

impl Default for MemoryCompressor<EmbeddingSimilarityDetector> {
    fn default() -> Self {
        Self::new(EmbeddingSimilarityDetector::default())
    }
}

fn estimate_tokens(content: &str) -> usize {
    content.chars().count().max(1).div_ceil(4)
}

fn extract_key_facts(entries: &[MemoryEntry]) -> HashSet<String> {
    let mut facts = HashSet::new();
    for entry in entries {
        for token in entry.content.split_whitespace() {
            let normalized = token
                .trim_matches(|c: char| !c.is_ascii_alphanumeric())
                .to_ascii_lowercase();
            if normalized.len() >= 4 {
                facts.insert(normalized);
            }
        }
    }
    facts
}

fn entry_similarity(left: &MemoryEntry, right: &MemoryEntry) -> f64 {
    if let (Some(a), Some(b)) = (
        extract_embedding_from_content(&left.content),
        extract_embedding_from_content(&right.content),
    ) {
        if a.len() == b.len() && !a.is_empty() {
            return f64::from(crate::memory::vector::cosine_similarity(&a, &b));
        }
    }
    char_ngram_jaccard(&left.content, &right.content, 3)
}

fn extract_embedding_from_content(content: &str) -> Option<Vec<f32>> {
    let parsed = serde_json::from_str::<serde_json::Value>(content).ok()?;
    let items = parsed.get("embedding")?.as_array()?;
    if items.is_empty() {
        return None;
    }

    let mut vec = Vec::with_capacity(items.len());
    for value in items {
        let number = value.as_f64()?;
        if !number.is_finite() {
            return None;
        }
        #[allow(clippy::cast_possible_truncation)]
        vec.push(number as f32);
    }
    Some(vec)
}

fn char_ngram_jaccard(left: &str, right: &str, n: usize) -> f64 {
    let left_grams = char_ngrams(left, n);
    let right_grams = char_ngrams(right, n);

    if left_grams.is_empty() && right_grams.is_empty() {
        return 1.0;
    }

    let intersection = left_grams.intersection(&right_grams).count();
    let union = left_grams.union(&right_grams).count();
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

fn char_ngrams(content: &str, n: usize) -> HashSet<String> {
    if n == 0 {
        return HashSet::new();
    }
    let normalized = content.to_ascii_lowercase();
    let chars = normalized.chars().collect::<Vec<_>>();
    if chars.is_empty() {
        return HashSet::new();
    }
    if chars.len() < n {
        return HashSet::from([normalized]);
    }

    let mut grams = HashSet::new();
    for window in chars.windows(n) {
        grams.insert(window.iter().collect::<String>());
    }
    grams
}

fn should_keep_left(left: &MemoryEntry, right: &MemoryEntry) -> bool {
    match (parse_ts(&left.timestamp), parse_ts(&right.timestamp)) {
        (Some(left_ts), Some(right_ts)) => {
            if left_ts == right_ts {
                left.id <= right.id
            } else {
                left_ts > right_ts
            }
        }
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => left.id <= right.id,
    }
}

fn parse_ts(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw).ok().map(|dt| dt.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, content: &str) -> MemoryEntry {
        entry_at(id, content, "2026-02-24T00:00:00Z")
    }

    fn entry_at(id: &str, content: &str, timestamp: &str) -> MemoryEntry {
        MemoryEntry {
            id: id.into(),
            key: id.into(),
            content: content.into(),
            category: MemoryCategory::Core,
            timestamp: timestamp.into(),
            session_id: None,
            score: None,
            tags: None,
            access_count: None,
            useful_count: None,
            source: None,
            source_confidence: None,
            verification_status: None,
            lifecycle_state: Some(LifecycleState::Active),
            compressed_from: None,
        }
    }

    #[test]
    fn should_trigger_when_limits_exceeded() {
        let compressor = MemoryCompressor::default();
        let entries = vec![
            entry("a", "alpha beta gamma delta epsilon"),
            entry("b", "alpha beta gamma delta epsilon"),
        ];
        let limits = CompressionLimits {
            max_entries: 1,
            max_tokens: 100,
        };
        assert!(compressor.should_trigger(&entries, limits));
    }

    #[test]
    fn finalize_rejects_high_fact_loss() {
        let compressor = MemoryCompressor::default();
        let originals = vec![
            entry("a", "Rust memory safety ownership"),
            entry("b", "Tokio async runtime scheduler"),
        ];
        let candidate =
            compressor.build_compressed_entry("compressed", "Rust summary only", MemoryCategory::Core, &originals);
        let result = compressor.finalize_compression(&originals, candidate).unwrap();
        assert!(!result.accepted);
        assert!(result.report.loss_rate > 0.10);
    }

    #[test]
    fn build_compressed_entry_sets_audit_chain() {
        let compressor = MemoryCompressor::default();
        let originals = vec![entry("src-1", "alpha"), entry("src-2", "beta")];
        let compressed =
            compressor.build_compressed_entry("compressed", "alpha beta", MemoryCategory::Core, &originals);
        assert_eq!(
            compressed.compressed_from,
            Some(vec!["src-1".to_string(), "src-2".to_string()])
        );
    }

    #[test]
    fn embedding_similarity_detector_clamps_threshold() {
        let low = EmbeddingSimilarityDetector::new(0.2);
        let high = EmbeddingSimilarityDetector::new(1.5);
        assert!((low.threshold - 0.8).abs() < f64::EPSILON);
        assert!((high.threshold - 0.99).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn embedding_similarity_detector_marks_older_duplicate() {
        let detector = EmbeddingSimilarityDetector::default();
        let entries = vec![
            entry_at(
                "old",
                "release validation completed with all checks passing",
                "2026-02-24T01:00:00Z",
            ),
            entry_at(
                "new",
                "release validation completed with all checks passing",
                "2026-02-25T01:00:00Z",
            ),
        ];
        let redundant = detector.detect_redundant_ids(&entries).await.unwrap();
        assert_eq!(redundant, vec!["old".to_string()]);
    }

    #[tokio::test]
    async fn embedding_similarity_detector_keeps_distinct_entries() {
        let detector = EmbeddingSimilarityDetector::default();
        let entries = vec![
            entry("a", "network timeout while connecting to service"),
            entry("b", "updated quarterly budget and forecast numbers"),
        ];
        let redundant = detector.detect_redundant_ids(&entries).await.unwrap();
        assert!(redundant.is_empty());
    }

    #[tokio::test]
    async fn embedding_similarity_detector_uses_embedding_payload_when_available() {
        let detector = EmbeddingSimilarityDetector::new(0.98);
        let entries = vec![
            entry_at(
                "older",
                r#"{"embedding":[1.0,0.0,0.0],"payload":"alpha notes"}"#,
                "2026-02-20T00:00:00Z",
            ),
            entry_at(
                "newer",
                r#"{"embedding":[1.0,0.0,0.0],"payload":"completely different text"}"#,
                "2026-02-21T00:00:00Z",
            ),
        ];
        let redundant = detector.detect_redundant_ids(&entries).await.unwrap();
        assert_eq!(redundant, vec!["older".to_string()]);
    }
}
