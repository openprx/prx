//! Offline guards for the `PostgreSQL` memory adapter's pgvector SQL (PG-VECTOR).
//!
//! These tests read `src/memory/postgres.rs` and every file under
//! `src/memory/postgres/` as text and need no database.
//!
//! Why: the adapter binds each embedding as the Rust `String` produced by
//! `pgvector_literal` (`[0.1,0.2,...]`). The `postgres` client lets the server
//! type every `$N`, so `$1::vector` types parameter 1 as `vector`, and a
//! `String` cannot be bound to that type: every vector write, ANN search and
//! reindex failed at bind time. `$1::text::vector` types the parameter as
//! `text` (which a `String` binds natively) and lets `PostgreSQL` run the
//! explicit text-to-vector input cast.
//!
//! What these guards do NOT prove: that a live server accepts the cast, that
//! the HNSW index is still chosen, or that the RLS transaction context
//! survives. Those need a real `PostgreSQL` with pgvector (the ignored,
//! env-gated tests in `src/memory/postgres/tests/`).

#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::unwrap_used)]

/// Number of `$N::text::vector` parameter casts in the adapter: seven
/// statements, two of which (the memory and document-chunk ANN searches) use
/// the query vector twice (score and `ORDER BY`).
const EXPECTED_TEXT_VECTOR_PARAMETERS: usize = 9;

const LIVE_URL_ENV_READ: &str = "std::env::var(\"OPENPRX_TEST_POSTGRES_URL\")";
/// The variable's name as a string literal, however it is then read.
const LIVE_URL_ENV_NAME: &str = "\"OPENPRX_TEST_POSTGRES_URL\"";
/// The attribute every live test carries; the count below counts these.
const LIVE_TEST_IGNORE: &str = "#[ignore = \"needs a live PostgreSQL";
const LIVE_URL_HELPER_DEFINITION: &str = "pub(crate) fn live_postgres_url() -> String {";
const LIVE_URL_HELPER_CALL: &str = "live_postgres_url()";

/// Tests that need a live `PostgreSQL`, counted by their `#[ignore]` attribute:
/// 26 in the memory adapter, 4 in the cron
/// store, 1 in the cron scheduler, 2 in the webhook repository, 1 in schema
/// migration and 1 in the fitness CLI.
const EXPECTED_LIVE_POSTGRES_TESTS: usize = 36;

fn line_of(source: &str, offset: usize) -> usize {
    source[..offset].matches('\n').count() + 1
}

/// Every file of the PostgreSQL memory adapter with its path relative to the
/// manifest directory: the root `src/memory/postgres.rs` plus everything
/// under `src/memory/postgres/`. The adapter used to be one file, so a guard
/// that read only the root would go blind to every statement that moved out.
fn postgres_memory_sources() -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<(String, String)>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .expect("test: read the PostgreSQL adapter directory")
            .map(|entry| entry.expect("test: read a directory entry").path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let relative = path.strip_prefix(root).expect("test: path under the root");
                let text = std::fs::read_to_string(&path).expect("test: read an adapter source file");
                out.push((relative.display().to_string(), text));
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let root_file = root.join("src/memory/postgres.rs");
    let mut out = vec![(
        "src/memory/postgres.rs".to_string(),
        std::fs::read_to_string(&root_file).expect("test: read src/memory/postgres.rs"),
    )];
    walk(&root.join("src/memory/postgres"), root, &mut out);
    assert!(
        out.len() > 2 && out.iter().all(|(_, text)| !text.is_empty()),
        "the PostgreSQL adapter scan must see the root file and its directory: {:?}",
        out.iter().map(|(path, _)| path).collect::<Vec<_>>()
    );
    out
}

/// Split `text` into (rest, `$N` placeholder) when it ends with one.
fn strip_trailing_placeholder(text: &str) -> Option<&str> {
    let digits = text.len() - text.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }
    text[..text.len() - digits].strip_suffix('$')
}

#[derive(Debug, PartialEq, Eq)]
enum VectorCast {
    /// `$N::text::vector` — the parameter is typed `text`.
    TextParameter,
    /// `$N::vector` — the parameter is typed `vector`; a `String` cannot bind.
    VectorParameter,
    /// Anything else (a literal cast, a column cast, ...) — needs review.
    Other,
}

fn vector_casts(source: &str) -> Vec<(usize, VectorCast)> {
    source
        .match_indices("::vector")
        // A cast names the type; `Self::vector_rls_policy_sql` and the like
        // are Rust paths, not casts.
        .filter(|(offset, cast)| {
            !source[offset + cast.len()..]
                .starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
        })
        .map(|(offset, _)| {
            let before = &source[..offset];
            let (text_first, operand) = before
                .strip_suffix("::text")
                .map_or((false, before), |rest| (true, rest));
            let kind = match (text_first, strip_trailing_placeholder(operand).is_some()) {
                (true, true) => VectorCast::TextParameter,
                (false, true) => VectorCast::VectorParameter,
                (_, false) => VectorCast::Other,
            };
            (line_of(source, offset), kind)
        })
        .collect()
}

#[test]
fn pgvector_parameters_are_never_typed_as_vector() {
    let offending: Vec<String> = postgres_memory_sources()
        .iter()
        .flat_map(|(path, text)| {
            vector_casts(text)
                .into_iter()
                .filter(|(_, kind)| *kind != VectorCast::TextParameter)
                .map(move |(line, kind)| format!("{path}:{line} ({kind:?})"))
        })
        .collect();
    assert!(
        offending.is_empty(),
        "every `::vector` cast in the PostgreSQL adapter must be a `$N::text::vector` \
         parameter cast; a bare `$N::vector` types the parameter as `vector`, which the \
         `String` from `pgvector_literal` cannot bind to. Offending casts: {offending:#?}"
    );
}

#[test]
fn pgvector_parameters_still_reach_the_vector_type() {
    let text_parameters: usize = postgres_memory_sources()
        .iter()
        .map(|(_, text)| {
            vector_casts(text)
                .into_iter()
                .filter(|(_, kind)| *kind == VectorCast::TextParameter)
                .count()
        })
        .sum();
    assert_eq!(
        text_parameters, EXPECTED_TEXT_VECTOR_PARAMETERS,
        "expected {EXPECTED_TEXT_VECTOR_PARAMETERS} `$N::text::vector` casts (seven statements: \
         two stores, two ANN searches using the query vector twice, one chunk ingest, two \
         reindex updates). A lower count means a vector path was dropped; update this \
         constant only together with a reviewed new or removed vector statement."
    );
}

#[test]
fn pgvector_literal_stays_the_text_form_bound_to_those_parameters() {
    // The `::text` cast is correct only while the bound value is text. If the
    // producer changes to a native vector type (e.g. the `pgvector` crate), the
    // casts above must be revisited together with it.
    let sources = postgres_memory_sources();
    assert!(
        sources
            .iter()
            .any(|(_, text)| text.contains("fn pgvector_literal(embedding: &[f32]) -> Option<String> {")),
        "`pgvector_literal` must keep producing the text form (`Option<String>`) that the \
         `$N::text::vector` parameters are typed for"
    );
    assert!(
        !sources.iter().any(|(_, text)| text.contains("pgvector::")),
        "the adapter binds vectors as text; a native pgvector binding needs the casts revisited"
    );
}

/// Every Rust source file of the crate and its integration tests
/// (`src/**/*.rs`, `tests/**/*.rs`) with its path relative to the manifest
/// directory. This guard's own file is left out: it names the helper and the
/// variable as data.
fn crate_sources() -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<(String, String)>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .expect("test: read a source directory")
            .map(|entry| entry.expect("test: read a directory entry").path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let relative = path.strip_prefix(root).expect("test: path under the root");
                let relative = relative.display().to_string();
                if relative.ends_with("pgvector_binding_regression.rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("test: read a source file");
                out.push((relative, text));
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut out = Vec::new();
    walk(&root.join("src"), root, &mut out);
    walk(&root.join("tests"), root, &mut out);
    out
}

/// Whether `line` declares a function, at any indentation and with any
/// visibility or `async`/`unsafe`/`const` qualifier.
fn is_fn_line(line: &str) -> bool {
    let mut rest = line.trim_start();
    loop {
        let stripped = ["pub(crate) ", "pub(super) ", "pub ", "async ", "unsafe ", "const "]
            .iter()
            .find_map(|prefix| rest.strip_prefix(prefix));
        match stripped {
            Some(next) => rest = next,
            None => return rest.starts_with("fn "),
        }
    }
}

/// The `fn` line enclosing `offset` — the nearest function declaration above
/// it, at any indentation — and the attribute and comment lines directly
/// above that declaration.
fn enclosing_fn_header(source: &str, offset: usize) -> (String, Vec<&str>) {
    let before = &source[..offset];
    let fn_start = before
        .match_indices('\n')
        .map(|(index, _)| index + 1)
        .rev()
        .find(|&start| is_fn_line(before[start..].lines().next().unwrap_or_default()))
        .expect("test: a live PostgreSQL helper call sits inside a fn");
    let fn_line = source[fn_start..].lines().next().unwrap_or_default().trim().to_string();
    let attributes = source[..fn_start]
        .lines()
        .rev()
        .take_while(|line| {
            let line = line.trim();
            line.starts_with("#[") || line.starts_with("//")
        })
        .collect();
    (fn_line, attributes)
}

/// What the scan finds in a set of sources: the `#[ignore = "needs a live
/// PostgreSQL ..."]` tests, the helper call sites that are not inside such a
/// test, and every place that names the variable outside the helper.
#[derive(Debug, Default)]
struct LiveTestScan {
    ignored_live_tests: Vec<String>,
    unignored_calls: Vec<String>,
    stray_variable_names: Vec<String>,
}

fn scan_live_postgres_tests(sources: &[(String, String)]) -> LiveTestScan {
    let mut scan = LiveTestScan::default();
    for (path, text) in sources {
        scan.ignored_live_tests.extend(
            text.match_indices(LIVE_TEST_IGNORE)
                .map(|(offset, _)| format!("{path}:{}", line_of(text, offset))),
        );
        let helper_body = text
            .split_once(LIVE_URL_HELPER_DEFINITION)
            .and_then(|(head, rest)| rest.split_once("\n}\n").map(|(body, _)| (head.len(), body.len())));
        for (offset, _) in text.match_indices(LIVE_URL_ENV_NAME) {
            let inside_helper = helper_body
                .is_some_and(|(start, len)| offset > start && offset < start + LIVE_URL_HELPER_DEFINITION.len() + len);
            if !inside_helper {
                scan.stray_variable_names
                    .push(format!("{path}:{}", line_of(text, offset)));
            }
        }
        for (offset, _) in text.match_indices(LIVE_URL_HELPER_CALL) {
            if text[..offset].ends_with("fn ") {
                continue;
            }
            let (fn_line, attributes) = enclosing_fn_header(text, offset);
            let is_test = attributes
                .iter()
                .any(|line| line.trim().starts_with("#[tokio::test") || line.trim().starts_with("#[test]"));
            let is_ignored = attributes.iter().any(|line| line.trim().starts_with(LIVE_TEST_IGNORE));
            if !(is_test && is_ignored) {
                scan.unignored_calls.push(format!("{path}: {fn_line}"));
            }
        }
    }
    scan
}

#[test]
fn live_postgres_tests_report_as_ignored_rather_than_passing() {
    let sources = crate_sources();
    let helper_file = sources
        .iter()
        .find(|(path, _)| path.ends_with("src/memory/postgres.rs"))
        .map(|(_, text)| text.as_str())
        .expect("test: src/memory/postgres.rs is a crate source");
    let helper_body_has_the_read = helper_file
        .split_once(LIVE_URL_HELPER_DEFINITION)
        .and_then(|(_, rest)| rest.split_once("\n}\n"))
        .is_some_and(|(body, _)| body.contains(LIVE_URL_ENV_READ));
    assert!(
        helper_body_has_the_read,
        "the live PostgreSQL URL helper must exist in src/memory/postgres.rs and read the variable"
    );

    let scan = scan_live_postgres_tests(&sources);
    assert!(
        scan.stray_variable_names.is_empty(),
        "only `live_postgres_url` may name OPENPRX_TEST_POSTGRES_URL; any other read — direct, \
         through a constant, in src/ or tests/ — lets a test return early when it is unset and \
         report a skip as a pass: {:#?}",
        scan.stray_variable_names
    );
    assert_eq!(
        scan.ignored_live_tests.len(),
        EXPECTED_LIVE_POSTGRES_TESTS,
        "live PostgreSQL tests changed; update the count with the review: {:#?}",
        scan.ignored_live_tests
    );
    assert!(
        scan.unignored_calls.is_empty(),
        "every test that needs a live PostgreSQL must be `#[ignore = \"needs a live PostgreSQL ...\"]` \
         so an unset OPENPRX_TEST_POSTGRES_URL is counted as ignored, not passed: {:#?}",
        scan.unignored_calls
    );
}

/// R-B2 F-2: the scan attributes a helper call to the function that really
/// encloses it, at any indentation, and catches a variable read through a
/// constant. The old scan looked only for 4-space `fn` lines, so a call in a
/// nested module's un-ignored test borrowed the previous ignored test's
/// attributes and passed.
#[test]
fn the_live_test_scan_sees_nested_fns_and_indirect_reads() {
    let nested = concat!(
        "mod tests {\n",
        "    #[tokio::test]\n",
        "    #[ignore = \"needs a live PostgreSQL: set it\"]\n",
        "    async fn outer_live() {\n",
        "        let _ = live_postgres_url();\n",
        "    }\n",
        "    mod nested {\n",
        "        #[tokio::test]\n",
        "        async fn nested_unignored() {\n",
        "            let _ = live_postgres_url();\n",
        "        }\n",
        "    }\n",
        "}\n",
    );
    let indirect = "const V: &str = \"OPENPRX_TEST_POSTGRES_URL\";\nfn f() { let _ = std::env::var(V); }\n";
    let scan = scan_live_postgres_tests(&[
        ("src/fixture.rs".to_string(), nested.to_string()),
        ("tests/fixture.rs".to_string(), indirect.to_string()),
    ]);
    assert_eq!(scan.ignored_live_tests.len(), 1, "{scan:#?}");
    assert_eq!(
        scan.unignored_calls,
        vec!["src/fixture.rs: async fn nested_unignored() {".to_string()],
        "{scan:#?}"
    );
    assert_eq!(
        scan.stray_variable_names,
        vec!["tests/fixture.rs:1".to_string()],
        "{scan:#?}"
    );
}
