use crate::memory::{LifecycleState, MemoryCategory, MemoryEntry};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use std::collections::HashSet;
use uuid::Uuid;

/// Compression trigger limits.
#[derive(Debug, Clone, Copy)]
pub struct CompressionLimits {
    pub max_entries: usize,
    pub max_tokens: usize,
}

/// Similarity detector extension point reserved for future implementation.
#[async_trait]
pub trait SimilarityDetector: Send + Sync {
    async fn detect_redundant_ids(&self, _entries: &[MemoryEntry]) -> Result<Vec<String>>;
}

/// Default placeholder detector with no-op behavior.
pub struct DefaultSimilarityDetector;

#[async_trait]
impl SimilarityDetector for DefaultSimilarityDetector {
    async fn detect_redundant_ids(&self, _entries: &[MemoryEntry]) -> Result<Vec<String>> {
        Ok(Vec::new())
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

    /// Reserved redundancy detector hook.
    pub async fn detect_redundancy(&self, entries: &[MemoryEntry]) -> Result<Vec<String>> {
        self.detector.detect_redundant_ids(entries).await
    }

    /// Verify critical fact retention. Reject when loss rate exceeds 10%.
    pub fn verify_fidelity(
        &self,
        originals: &[MemoryEntry],
        compressed_content: &str,
    ) -> FidelityReport {
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
    pub fn finalize_compression(
        &self,
        originals: &[MemoryEntry],
        candidate: MemoryEntry,
    ) -> Result<CompressionResult> {
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

impl Default for MemoryCompressor<DefaultSimilarityDetector> {
    fn default() -> Self {
        Self::new(DefaultSimilarityDetector)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, content: &str) -> MemoryEntry {
        MemoryEntry {
            id: id.into(),
            key: id.into(),
            content: content.into(),
            category: MemoryCategory::Core,
            timestamp: "2026-02-24T00:00:00Z".into(),
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
        let candidate = compressor.build_compressed_entry(
            "compressed",
            "Rust summary only",
            MemoryCategory::Core,
            &originals,
        );
        let result = compressor
            .finalize_compression(&originals, candidate)
            .unwrap();
        assert!(!result.accepted);
        assert!(result.report.loss_rate > 0.10);
    }

    #[test]
    fn build_compressed_entry_sets_audit_chain() {
        let compressor = MemoryCompressor::default();
        let originals = vec![entry("src-1", "alpha"), entry("src-2", "beta")];
        let compressed = compressor.build_compressed_entry(
            "compressed",
            "alpha beta",
            MemoryCategory::Core,
            &originals,
        );
        assert_eq!(
            compressed.compressed_from,
            Some(vec!["src-1".to_string(), "src-2".to_string()])
        );
    }
}
