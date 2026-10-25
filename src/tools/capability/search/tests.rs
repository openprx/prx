use super::*;
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogSources, SurfacePolicy};
use crate::tools::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::{Value, json};

struct SearchFixtureTool {
    name: String,
    description: String,
}

#[async_trait]
impl Tool for SearchFixtureTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"query":{"type":"string"}}})
    }
    async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
        anyhow::bail!("Search fixtures cannot execute")
    }
}

fn registry(extra: usize, reverse: bool, varied: bool) -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = (0..=extra)
        .map(|position| {
            let description = if varied {
                match position % 4 {
                    0 => "alpha beta evidence",
                    1 => "beta beta evidence extra",
                    2 => "alpha alpha alpha evidence",
                    _ => "alpha beta beta evidence longer summary",
                }
            } else {
                "alpha beta evidence"
            };
            Box::new(SearchFixtureTool {
                name: format!("prx_doc_{position:04}"),
                description: description.to_owned(),
            }) as Box<dyn Tool>
        })
        .collect();
    if reverse {
        tools.reverse();
    }
    tools
}

fn assert_same(actual: &[SearchHit<'_>], expected: &[SearchHit<'_>]) {
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(expected) {
        assert!(std::ptr::eq(actual.entry, expected.entry));
        assert_eq!(actual.entry.id, expected.entry.id);
        assert_eq!(actual.score.to_bits(), expected.score.to_bits());
    }
}

#[test]
#[allow(clippy::print_stdout)] // Raw allocation attempts are retained acceptance evidence.
fn ranked_search_allocations_do_not_grow_with_matching_inventory() {
    let mut counts = Vec::new();
    for extra in [0, 1, 16, 64, 256, 512] {
        let tools = registry(extra, false, false);
        let catalog = CapabilityCatalog::build(&tools, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let view = catalog.view(&CatalogScope::default());
        assert_eq!(view.iter_entries().count(), extra + 1);
        for token in ["alpha", "beta", "evidence"] {
            let postings = view.search_index.postings.get(token).unwrap();
            assert_eq!(postings.len(), extra + 1);
            assert!(postings.iter().all(|(position, _)| view.is_visible_index(*position)));
        }
        let expected = legacy_find(&view, "beta alpha evidence", &[], 1);
        assert_eq!(expected.first().unwrap().entry.public_name, "prx_doc_0000");
        let (cold_hits, cold_count) = crate::decision::builtin::allocation_counter::count(|| {
            CatalogSearch::find(&view, "beta alpha evidence", &[], 1)
        });
        assert_same(&cold_hits, &expected);
        std::hint::black_box(&cold_hits);
        let (hits, count) = crate::decision::builtin::allocation_counter::count(|| {
            CatalogSearch::find(&view, "beta alpha evidence", &[], 1)
        });
        assert_same(&hits, &expected);
        std::hint::black_box(&hits);
        println!("PRX_SEARCH_ALLOCATION extra={extra} cold_attempts={cold_count} warm_attempts={count}");
        counts.push((extra, cold_count, count));
    }
    let baseline = counts.first().unwrap().1;
    for (extra, cold_count, count) in counts {
        assert_eq!(
            cold_count, baseline,
            "First query allocations must stay bounded at N={extra}"
        );
        assert_eq!(
            count, baseline,
            "Matching inventory must not grow query allocations at N={extra}"
        );
    }
}

#[test]
fn ranked_search_matches_original_score_bits_and_order() {
    for extra in [0, 1, 16, 64, 256, 512] {
        for reverse in [false, true] {
            for varied in [false, true] {
                let tools = registry(extra, reverse, varied);
                let surface = SurfacePolicy {
                    allowed_tools: None,
                    excluded_names: BTreeSet::from(["prx_doc_0001".to_owned()]),
                    channel_allowed: true,
                    ..SurfacePolicy::default()
                };
                let catalog = CapabilityCatalog::build(&tools, &CatalogSources::default(), &surface).unwrap();
                for channel in [false, true] {
                    for scope_families in [
                        BTreeSet::new(),
                        BTreeSet::from([CapabilityTarget::NativeTool]),
                        BTreeSet::from([CapabilityTarget::Mcp]),
                    ] {
                        let view = catalog.view(&CatalogScope {
                            families: scope_families,
                            channel,
                        });
                        for initialized in [false, true] {
                            if initialized {
                                let _ = view.entries();
                            }
                            for query in [
                                "",
                                " ",
                                "...",
                                "no_match",
                                "beta alpha evidence",
                                "alpha alpha beta",
                                "evidence alpha",
                                "prx_doc_0000",
                                "PRX_DOC_0001",
                            ] {
                                for families in [
                                    &[][..],
                                    &[CapabilityTarget::NativeTool][..],
                                    &[CapabilityTarget::Mcp][..],
                                    &[CapabilityTarget::NativeTool, CapabilityTarget::Mcp][..],
                                ] {
                                    for limit in [0, 1, 3, 63, 64, 65, 128] {
                                        assert_same(
                                            &CatalogSearch::find(&view, query, families, limit),
                                            &legacy_find(&view, query, families, limit),
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

// Frozen pre-remediation scorer: same-catalog comparisons require exact score bits.
fn legacy_find<'a>(
    view: &CatalogView<'a>,
    query: &str,
    families: &[CapabilityTarget],
    limit: usize,
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
                .is_some_and(|entry| families.is_empty() || families.contains(&entry.family))
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
    let mut scores = BTreeMap::<usize, f64>::new();
    for token in query_tokens {
        let Some(postings) = index.postings.get(&token) else {
            continue;
        };
        let df = postings.len() as f64;
        let idf = ((count - df + 0.5) / (df + 0.5)).ln_1p();
        for (position, frequency) in postings {
            if !allowed(*position) {
                continue;
            }
            let Some(length) = index.lengths.get(*position) else {
                continue;
            };
            let tf = *frequency as f64;
            let score = idf * tf * 2.2 / 1.2_f64.mul_add(0.25 + 0.75 * *length as f64 / index.average_length, tf);
            *scores.entry(*position).or_default() += score;
        }
    }
    let mut ranked: Vec<_> = scores
        .into_iter()
        .filter(|(_, score)| *score > 0.0)
        .filter_map(|(position, score)| view.all_entries.get(position).map(|entry| SearchHit { entry, score }))
        .collect();
    ranked.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.entry.id.cmp(&right.entry.id))
    });
    ranked.truncate(limit);
    ranked
}

#[test]
fn ranked_search_preserves_mixed_alias_and_tokenizer_contracts() {
    use crate::tools::mcp::McpCapabilitySpec;
    use crate::tools::traits::ToolSpec;
    let sources = CatalogSources {
        mcp: vec![
            McpCapabilitySpec {
                server: "prx_server".into(),
                tool: "shared_alias".into(),
                state_digest: [0; 32],
                spec: ToolSpec {
                    name: "mcp__prx_server__first".into(),
                    description: "alpha beta evidence".into(),
                    parameters: json!({"type":"object","properties":{}}),
                },
            },
            McpCapabilitySpec {
                server: "prx_server".into(),
                tool: "shared_alias".into(),
                state_digest: [0; 32],
                spec: ToolSpec {
                    name: "mcp__prx_server__second".into(),
                    description: "beta alpha alpha evidence".into(),
                    parameters: json!({"type":"object","properties":{}}),
                },
            },
            McpCapabilitySpec {
                server: "prx_server".into(),
                tool: "mcp__prx_server__duplicate".into(),
                state_digest: [0; 32],
                spec: ToolSpec {
                    name: "mcp__prx_server__duplicate".into(),
                    description: "beta evidence".into(),
                    parameters: json!({"type":"object","properties":{}}),
                },
            },
        ],
        ..CatalogSources::default()
    };
    let mut tools = registry(65, false, true);
    tools.push(Box::new(SearchFixtureTool {
        name: "prx_script".into(),
        description: "alpha r\u{e9}sum\u{e9} file beta \u{e9}vidence".into(),
    }));
    for channel_allowed in [false, true] {
        let surface = SurfacePolicy {
            allowed_tools: None,
            channel_allowed,
            excluded_names: BTreeSet::from(["prx_doc_0001".into()]),
            ..SurfacePolicy::default()
        };
        let catalog = CapabilityCatalog::build(&tools, &sources, &surface).unwrap();
        for channel in [false, true] {
            for scope_families in [
                BTreeSet::new(),
                BTreeSet::from([CapabilityTarget::NativeTool]),
                BTreeSet::from([CapabilityTarget::Mcp]),
                BTreeSet::from([CapabilityTarget::NativeTool, CapabilityTarget::Mcp]),
            ] {
                let view = catalog.view(&CatalogScope {
                    channel,
                    families: scope_families,
                });
                let mut queries = vec![
                    "alpha beta evidence".to_owned(),
                    "beta alpha alpha".into(),
                    "shared_alias".into(),
                    "SHARED_ALIAS".into(),
                    "mcp__prx_server__duplicate".into(),
                    "prx_doc_0001".into(),
                    "r\u{e9}sum\u{e9}".into(),
                    "alphar\u{e9}sum\u{e9}beta".into(),
                    "\u{e9}vidence".into(),
                ];
                queries.extend(view.all_entries.iter().map(|entry| entry.id.as_str().to_owned()));
                for query in queries {
                    for families in [
                        &[][..],
                        &[CapabilityTarget::NativeTool][..],
                        &[CapabilityTarget::Mcp][..],
                        &[CapabilityTarget::NativeTool, CapabilityTarget::Mcp][..],
                    ] {
                        for limit in [0, 1, 3, 63, 64, 65, 128] {
                            assert_same(
                                &CatalogSearch::find(&view, &query, families, limit),
                                &legacy_find(&view, &query, families, limit),
                            );
                        }
                    }
                }
                if !channel || channel_allowed {
                    let duplicate = CatalogSearch::find(&view, "mcp__prx_server__duplicate", &[], 3);
                    if view
                        .iter_entries()
                        .any(|entry| entry.public_name == "mcp__prx_server__duplicate")
                    {
                        assert_eq!(
                            duplicate.len(),
                            2,
                            "The name/alias fast path preserves duplicate positions"
                        );
                        assert!(std::ptr::eq(
                            duplicate.first().unwrap().entry,
                            duplicate.get(1).unwrap().entry
                        ));
                    }
                } else {
                    assert!(CatalogSearch::find(&view, "alpha beta evidence", &[], 64).is_empty());
                }
            }
        }
    }
    let empty = CapabilityCatalog::build(&[], &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
    let view = empty.view(&CatalogScope::default());
    for query in ["", "alpha beta", "prx_doc_0000", "r\u{e9}sum\u{e9}"] {
        assert_same(
            &CatalogSearch::find(&view, query, &[], 64),
            &legacy_find(&view, query, &[], 64),
        );
    }
}

#[test]
fn durable_content_queries_discover_a_reader_with_real_memory_descriptions() {
    use std::sync::Arc;
    let workspace = tempfile::tempdir().unwrap();
    let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).unwrap());
    let security = Arc::new(crate::security::SecurityPolicy::default());
    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(crate::tools::memory_get::MemoryGetTool::new(
            workspace.path().to_owned(),
            Arc::clone(&memory),
            false,
        )),
        Box::new(crate::tools::memory_recall::MemoryRecallTool::new(
            Arc::clone(&memory),
            false,
        )),
        Box::new(crate::tools::memory_store::MemoryStoreTool::new(memory, security)),
    ];
    let catalog = CapabilityCatalog::build(&tools, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
    let view = catalog.view(&CatalogScope::default());
    for query in [
        "What content is stored under prx_probe in durable long-term memory? Retrieve the actual stored value; do not use workspace files or guess.",
        "Retrieve saved preferences from long-term memory",
        "Read durable memory by key",
    ] {
        let hits = CatalogSearch::find(&view, query, &[CapabilityTarget::Memory], 1);
        assert!(
            matches!(
                hits.first().map(|hit| hit.entry.public_name.as_str()),
                Some("memory_get" | "memory_recall")
            ),
            "{query}"
        );
    }
}
