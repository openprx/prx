use super::catalog::{CapabilityEntry, CatalogView, Exposure};
use crate::causal_tree::branch::CapabilityTarget;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

const MAX_HITS: usize = 64;

/// Character ranges of the scripts that write words without spaces between
/// them. This is a script-class list, not a language list: it only decides how
/// text is *counted*, never which capability a word names, so it cannot grow
/// into a multilingual keyword table.
const UNSPACED_SCRIPT_RANGES: &[(u32, u32)] = &[
    (0x0e00, 0x0eff),   // Thai, Lao
    (0x1000, 0x109f),   // Myanmar
    (0x1780, 0x17ff),   // Khmer
    (0x3040, 0x30ff),   // Hiragana, Katakana
    (0x3400, 0x4dbf),   // CJK unified ideographs extension A
    (0x4e00, 0x9fff),   // CJK unified ideographs
    (0xf900, 0xfaff),   // CJK compatibility ideographs
    (0x20000, 0x2fa1f), // CJK unified ideographs, supplementary planes
];

/// Whether this character belongs to a script that does not separate words
/// with spaces, and therefore has to be counted per character.
pub(crate) fn character_is_unspaced_script(character: char) -> bool {
    let code_point = u32::from(character);
    UNSPACED_SCRIPT_RANGES
        .iter()
        .any(|(first, last)| (*first..=*last).contains(&code_point))
}

pub(crate) struct SearchHit<'a> {
    pub entry: &'a CapabilityEntry,
    pub score: f64,
}

/// Built once per catalog generation. A scoped view changes only the visible
/// position set; query time visits matching postings, not the whole catalog.
#[derive(Debug, Clone)]
pub(super) struct SearchIndex {
    canonical: BTreeMap<String, Vec<usize>>,
    names: BTreeMap<String, Vec<usize>>,
    postings: BTreeMap<String, Vec<(usize, usize)>>,
    lengths: Vec<usize>,
    average_length: f64,
    indexed_count: usize,
}

impl SearchIndex {
    pub(super) fn build(entries: &[CapabilityEntry]) -> Self {
        let mut canonical = BTreeMap::<String, Vec<usize>>::new();
        let mut names = BTreeMap::<String, Vec<usize>>::new();
        let mut postings = BTreeMap::<String, Vec<(usize, usize)>>::new();
        let mut lengths = vec![0; entries.len()];
        let mut indexed_count = 0;
        for (index, entry) in entries.iter().enumerate() {
            if matches!(entry.exposure, Exposure::Unavailable { .. }) {
                continue;
            }
            indexed_count += 1;
            canonical.entry(entry.id.as_str().to_string()).or_default().push(index);
            names.entry(entry.public_name.to_lowercase()).or_default().push(index);
            for alias in &entry.aliases {
                names.entry(alias.to_lowercase()).or_default().push(index);
            }
            let tokens = document_tokens(entry);
            if let Some(length) = lengths.get_mut(index) {
                *length = tokens.len();
            }
            let mut frequencies = BTreeMap::<String, usize>::new();
            for token in tokens {
                *frequencies.entry(token).or_default() += 1;
            }
            for (token, frequency) in frequencies {
                postings.entry(token).or_default().push((index, frequency));
            }
        }
        let average_length = lengths.iter().sum::<usize>().max(1) as f64 / indexed_count.max(1) as f64;
        Self {
            canonical,
            names,
            postings,
            lengths,
            average_length,
            indexed_count,
        }
    }
}

pub(crate) struct CatalogSearch;

impl CatalogSearch {
    /// Exact bounded mentions only: no ranked search, catalog scan or
    /// discovery publication. Prefix probes borrow the frozen name indexes.
    /// Overflow refuses the request instead of silently narrowing its intent.
    pub(crate) fn exact_mentions<'a>(
        view: &CatalogView<'a>,
        query: &str,
        limit: usize,
    ) -> Option<Vec<&'a CapabilityEntry>> {
        use std::borrow::Cow;
        use std::ops::Bound::{Included, Unbounded};
        let folded = if query.chars().any(char::is_uppercase) {
            Cow::Owned(query.to_lowercase())
        } else {
            Cow::Borrowed(query)
        };
        let index = view.search_index;
        let has_prefix = |map: &BTreeMap<String, Vec<usize>>, prefix: &str| {
            map.range::<str, _>((Included(prefix), Unbounded))
                .next()
                .is_some_and(|(term, _)| term.starts_with(prefix))
        };
        let identifier = |character: char| character.is_alphanumeric() || character == '_';
        let mut found = Vec::new();
        // Canonical ids retain their exact spelling; public names and
        // aliases use the same case folding as ordinary catalog search.
        for (text, terms) in [(query, &index.canonical), (folded.as_ref(), &index.names)] {
            let mut before = None;
            for (start, character) in text.char_indices() {
                if before.is_some_and(identifier) {
                    before = Some(character);
                    continue;
                }
                before = Some(character);
                for (offset, last) in text[start..].char_indices() {
                    let end = start + offset + last.len_utf8();
                    let term = &text[start..end];
                    if !has_prefix(terms, term) {
                        break;
                    }
                    if text[end..].chars().next().is_some_and(identifier) {
                        continue;
                    }
                    for position in terms.get(term).into_iter().flatten() {
                        if !view.is_visible_index(*position) {
                            continue;
                        }
                        let entry = view.all_entries.get(*position)?;
                        if found.iter().any(|prior: &&CapabilityEntry| prior.id == entry.id) {
                            continue;
                        }
                        if found.len() == limit {
                            return None;
                        }
                        found.push(entry);
                    }
                }
            }
        }
        Some(found)
    }

    #[cfg(test)]
    pub(crate) fn find<'a>(
        view: &CatalogView<'a>,
        query: &str,
        families: &[CapabilityTarget],
        limit: usize,
    ) -> Vec<SearchHit<'a>> {
        Self::find_filtered(view, query, families, limit, |_| true)
    }

    /// Apply source eligibility before top-k selection so excluded matches
    /// cannot displace eligible results. Ranking still visits index postings.
    pub(crate) fn find_filtered<'a>(
        view: &CatalogView<'a>,
        query: &str,
        families: &[CapabilityTarget],
        limit: usize,
        eligible: impl Fn(&CapabilityEntry) -> bool,
    ) -> Vec<SearchHit<'a>> {
        let limit = limit.min(MAX_HITS);
        if limit == 0 || query.trim().is_empty() {
            return Vec::new();
        }
        let index = &view.search_index;
        let allowed = |position: usize| {
            view.is_visible_index(position)
                && view
                    .all_entries
                    .get(position)
                    .is_some_and(|entry| (families.is_empty() || families.contains(&entry.family)) && eligible(entry))
        };
        let exact = |positions: Option<&Vec<usize>>| {
            positions
                .into_iter()
                .flatten()
                .copied()
                .filter(|position| allowed(*position))
                .take(limit)
                .filter_map(|position| {
                    view.all_entries.get(position).map(|entry| SearchHit {
                        entry,
                        score: f64::INFINITY,
                    })
                })
                .collect::<Vec<_>>()
        };
        let canonical = exact(index.canonical.get(query.trim()));
        if !canonical.is_empty() {
            return canonical;
        }
        let public = exact(index.names.get(&query.trim().to_lowercase()));
        if !public.is_empty() {
            return public;
        }
        let query_tokens: BTreeSet<_> = tokenize(query).into_iter().collect();
        if query_tokens.is_empty() {
            return Vec::new();
        }
        let count = index.indexed_count as f64;
        // Cursors follow lexical query-token order, preserving the original
        // floating-point contribution order for each matching document.
        let mut cursors: Vec<_> = query_tokens
            .iter()
            .filter_map(|token| {
                let postings = index.postings.get(token)?;
                let df = postings.len() as f64;
                Some(PostingCursor {
                    postings,
                    next: 0,
                    idf: ((count - df + 0.5) / (df + 0.5)).ln_1p(),
                })
            })
            .collect();
        if cursors.is_empty() {
            return Vec::new();
        }
        let mut ranked: Vec<(usize, SearchHit<'a>)> = Vec::with_capacity(limit);
        while let Some(position) = cursors.iter().filter_map(PostingCursor::position).min() {
            let visible = allowed(position);
            let length = index.lengths.get(position);
            let mut score = 0.0;
            for cursor in &mut cursors {
                let Some(&(current, frequency)) = cursor.postings.get(cursor.next) else {
                    continue;
                };
                if current != position {
                    continue;
                }
                cursor.next += 1;
                if visible && let Some(length) = length {
                    let tf = frequency as f64;
                    score += cursor.idf * tf * 2.2
                        / 1.2_f64.mul_add(0.25 + 0.75 * *length as f64 / index.average_length, tf);
                }
            }
            if score.partial_cmp(&0.0) != Some(Ordering::Greater) || !visible {
                continue;
            }
            let Some(entry) = view.all_entries.get(position) else {
                continue;
            };
            let hit = (position, SearchHit { entry, score });
            let insertion = ranked.partition_point(|existing| ranked_order(existing, &hit) != Ordering::Greater);
            if insertion >= limit {
                continue;
            }
            // Remove before inserting so a full vector never grows past limit.
            if ranked.len() == limit {
                ranked.pop();
            }
            ranked.insert(insertion, hit);
        }
        ranked.into_iter().map(|(_, hit)| hit).collect()
    }
}

struct PostingCursor<'a> {
    postings: &'a [(usize, usize)],
    next: usize,
    idf: f64,
}

impl PostingCursor<'_> {
    fn position(&self) -> Option<usize> {
        self.postings.get(self.next).map(|(position, _)| *position)
    }
}

fn ranked_order(left: &(usize, SearchHit<'_>), right: &(usize, SearchHit<'_>)) -> Ordering {
    right
        .1
        .score
        .partial_cmp(&left.1.score)
        .unwrap_or(Ordering::Equal)
        .then_with(|| left.1.entry.id.cmp(&right.1.entry.id))
        .then_with(|| left.0.cmp(&right.0))
}

fn document_tokens(entry: &CapabilityEntry) -> Vec<String> {
    let mut text = format!("{} {}", entry.public_name, entry.summary);
    for alias in &entry.aliases {
        text.push(' ');
        text.push_str(alias);
    }
    if let Some(properties) = entry.schema.get("properties").and_then(serde_json::Value::as_object) {
        for name in properties.keys() {
            text.push(' ');
            text.push_str(name);
        }
    }
    tokenize(&text)
}

fn tokenize(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut spaced = String::new();
    let mut unspaced = Vec::new();
    for ch in text.chars() {
        if character_is_unspaced_script(ch) {
            if !spaced.is_empty() {
                words.push(std::mem::take(&mut spaced));
            }
            unspaced.push(ch);
        } else {
            append_unspaced_tokens(&mut words, &mut unspaced);
            if ch.is_alphanumeric() {
                spaced.extend(ch.to_lowercase());
            } else if !spaced.is_empty() {
                words.push(std::mem::take(&mut spaced));
            }
        }
    }
    append_unspaced_tokens(&mut words, &mut unspaced);
    if !spaced.is_empty() {
        words.push(spaced);
    }
    words
}

fn append_unspaced_tokens(words: &mut Vec<String>, characters: &mut Vec<char>) {
    for character in characters.iter() {
        words.push(character.to_string());
    }
    for pair in characters.windows(2) {
        words.push(pair.iter().collect());
    }
    characters.clear();
}

#[cfg(test)]
mod tests;
