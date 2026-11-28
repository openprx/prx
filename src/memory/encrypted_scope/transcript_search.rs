//! Transcript search inside the encrypted scope: a bounded decrypt scan over
//! the principal's session. The store holds this scope's rows sealed, so no
//! plaintext index of them exists; each row is opened here and matched in
//! memory.

use super::EncryptedScope;
use crate::memory::SessionContextQuery;
use crate::memory::traits::{MemoryPrincipal, TranscriptHit};
use crate::security::redaction::redact_secrets;
use std::collections::HashSet;
use std::sync::Arc;

/// Rows read from the store by one search, at most. A cost bound.
const SCAN_MAX_ROWS: usize = 2_000;
/// Rows requested per page of the scan.
const SCAN_PAGE_ROWS: usize = 200;
/// Same bounds as the SQLite transcript index: a row up to this many
/// characters is excerpted whole, a longer one around its first match.
const WHOLE_ROW_MAX_CHARS: usize = 65_536;
const REGION_SIDE_CHARS: usize = 4_096;

impl EncryptedScope {
    /// A handle on the same store, keys and scope that a blocking task can own.
    fn opener(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            crypto: Arc::clone(&self.crypto),
            account_id: self.account_id.clone(),
            scope: self.scope.clone(),
        }
    }

    /// Search the principal's session for rows that contain every word of
    /// `query`, newest first, at most `limit` hits. Only rows this scope can
    /// open are searched. `session_key`, when given, replaces the principal's
    /// session. A principal without a session searches nothing.
    pub(super) async fn search_sealed_transcripts(
        &self,
        principal: &MemoryPrincipal,
        query: &str,
        session_key: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<TranscriptHit>> {
        let words = distinct_words(query);
        let Some(anchor) = words.first().copied() else {
            return Ok(Vec::new());
        };
        if limit == 0 {
            return Ok(Vec::new());
        }
        let folded_words: Vec<String> = words.iter().map(|word| word.to_ascii_lowercase()).collect();
        let mut scan_principal = principal.clone();
        if let Some(session) = session_key {
            scan_principal.session_key = Some(session.to_owned());
        }
        let mut hits = Vec::new();
        let mut before_id = i64::MAX;
        let mut scanned = 0usize;
        while scanned < SCAN_MAX_ROWS && hits.len() < limit {
            let page_rows = SCAN_PAGE_ROWS.min(SCAN_MAX_ROWS - scanned);
            // Rows of other scopes are skipped below, not refused: the scan
            // makes no completeness claim, so it reads the inner store.
            let page = self
                .inner
                .load_session_context_before(
                    SessionContextQuery {
                        principal: scan_principal.clone(),
                        since_event_id: None,
                        limit: page_rows,
                        include_roles: vec!["user".to_string(), "assistant".to_string()],
                    },
                    before_id,
                )
                .await?;
            if page.is_empty() {
                break;
            }
            scanned += page.len();
            if let Some(oldest) = page.iter().map(|event| event.id).min() {
                before_id = oldest;
            }
            // Rows the store's own search leaves out for deletion (rf message
            // or reply deletes, including those of a still-live run whose rows
            // keep their sealed content, erased scopes, inactive incarnations)
            // are left out here too.
            let row_ids: Vec<i64> = page.iter().map(|event| event.id).collect();
            let excluded = self.inner.excluded_transcript_rows(&row_ids).await?;
            // Decrypt, redact and match off the async executor. Each row is
            // opened once and only the fields a hit prints are kept; the
            // payload columns are neither opened nor returned.
            let opener = self.opener();
            let folded = folded_words.clone();
            let anchor_owned = anchor.to_owned();
            let room = limit - hits.len();
            let page_hits = crate::runtime::blocking::spawn_blocking(move || {
                match_page(&opener, page, &excluded, &folded, &anchor_owned, room)
            })
            .await
            .map_err(|error| anyhow::anyhow!("sealed transcript scan task failed: {error}"))?;
            hits.extend(page_hits);
        }
        Ok(hits)
    }
}

/// The hits of one page, newest first, at most `room`. A row this scope cannot
/// open (another scope, or damaged) is skipped and the scan goes on.
fn match_page(
    opener: &EncryptedScope,
    page: Vec<crate::memory::MessageEvent>,
    excluded: &HashSet<i64>,
    folded_words: &[String],
    anchor: &str,
    room: usize,
) -> Vec<TranscriptHit> {
    let mut hits = Vec::new();
    for event in page {
        if hits.len() >= room {
            break;
        }
        if event.event_type != "message.created"
            || !matches!(event.role.as_str(), "user" | "assistant")
            || excluded.contains(&event.id)
        {
            continue;
        }
        let record_id = super::record_id_of(event.idempotency_key.as_deref(), &event.event_id);
        let Some(plain) = opener.open_record(&record_id, &event.content) else {
            continue;
        };
        // Redact the whole row first: a query matches the redacted text
        // only, so it cannot probe inside a secret.
        let redacted = redact_secrets(&plain);
        let lowered = redacted.to_ascii_lowercase();
        if !folded_words.iter().all(|word| lowered.contains(word.as_str())) {
            continue;
        }
        hits.push(TranscriptHit {
            session_key: event.session_key,
            event_id: event.event_id,
            role: event.role,
            created_at: event.created_at,
            excerpt: cut_excerpt(&redacted, anchor),
        });
    }
    hits
}

/// The query's whitespace-separated words, each once, in order; words are
/// compared ignoring ASCII case, as the SQLite search does.
fn distinct_words(query: &str) -> Vec<&str> {
    let mut seen = HashSet::new();
    query
        .split_whitespace()
        .filter(|word| seen.insert(word.to_ascii_lowercase()))
        .collect()
}

/// The excerpt of a redacted row: a row of at most [`WHOLE_ROW_MAX_CHARS`]
/// characters is returned whole; a longer one is cut to [`REGION_SIDE_CHARS`]
/// characters on each side of the first case-insensitive match of `anchor`, or
/// from its start when the match is not in the text (for example when it lay
/// inside a secret, which the text no longer shows).
fn cut_excerpt(redacted: &str, anchor: &str) -> String {
    let redacted: Vec<char> = redacted.chars().collect();
    if redacted.len() <= WHOLE_ROW_MAX_CHARS {
        return redacted.into_iter().collect();
    }
    let needle: Vec<char> = anchor.chars().map(|ch| ch.to_ascii_lowercase()).collect();
    let start = find_ascii_ignoring_case(&redacted, &needle).map_or(0, |at| at.saturating_sub(REGION_SIDE_CHARS));
    redacted
        .iter()
        .skip(start)
        .take(2 * REGION_SIDE_CHARS + needle.len())
        .collect()
}

/// Character index of the first occurrence of `needle` (already ASCII
/// lowercase) in `haystack`, comparing ASCII case-insensitively.
fn find_ascii_ignoring_case(haystack: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    haystack.windows(needle.len()).position(|window| {
        window
            .iter()
            .zip(needle)
            .all(|(ch, want)| ch.to_ascii_lowercase() == *want)
    })
}
