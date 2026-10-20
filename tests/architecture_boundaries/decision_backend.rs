use super::decision_seals::identifier_count;
use super::helpers::{
    blank_comments_and_literals, is_ident_char, read_source, relative_path, repository_root, rust_source_files,
    strip_cfg_test_items,
};
use super::tool_scanner::contains_identifier;
/// How many times `name` is named inside `use ..;` declarations of
/// `compact` (whitespace-normalized production text).
fn identifier_count_in_use_declarations(compact: &str, name: &str) -> usize {
    let mut count = 0;
    let mut search = 0;
    while let Some(found) = compact[search..].find("use ") {
        let start = search + found;
        search = start + "use ".len();
        if compact[..start].chars().next_back().is_some_and(is_ident_char) {
            continue;
        }
        let end = compact[start..]
            .find(';')
            .map_or(compact.len(), |offset| start + offset);
        count += identifier_count(&compact[start..end], name);
        search = end;
    }
    count
}

/// AG-04 (CDP-PLAN 1.4 and 1.5; D-079 L-11): the decision backend factory
/// is the only production code that validates a response, answers with the
/// builtin, builds a backend or asks one to evaluate, so no path around the
/// outbound guard, the breaker and validation exists.
///
/// - `DecisionResponseValidator::validate` and `BuiltinDecisionBackend::answer`
///   are named only in `src/decision/factory.rs`.
/// - `DecisionTransports` is named elsewhere only to import it, implement it
///   (`.. DecisionTransports for ..`), or as the turn service's single borrowed
///   constructor input. Only the factory invokes `build` on it.
/// - A file that names `DecisionBackend` outside the factory calls
///   `.evaluate(` only on `self` (a backend's own health probe).
#[test]
pub(super) fn only_the_factory_validates_answers_with_the_builtin_or_builds_a_backend() {
    const FACTORY: &str = "src/decision/factory.rs";
    let mut violations = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if relative == FACTORY {
            continue;
        }
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        let compact = production.split_whitespace().collect::<Vec<_>>().join(" ");
        for name in ["DecisionResponseValidator::validate", "BuiltinDecisionBackend::answer"] {
            if identifier_count(&compact, name) > 0 {
                violations.push(format!("{relative}: {name}"));
            }
        }
        let named = identifier_count(&compact, "DecisionTransports");
        let service_field = if relative == "src/runtime/turn_decision.rs" {
            let count = compact.matches("pub transports: &'a dyn DecisionTransports,").count();
            if count != 1 {
                violations.push(format!("{relative}: expected one borrowed constructor transport field"));
            }
            count
        } else {
            0
        };
        let allowed = identifier_count(&compact, "DecisionTransports for")
            + identifier_count_in_use_declarations(&compact, "DecisionTransports")
            + service_field;
        if named > allowed {
            violations.push(format!("{relative}: DecisionTransports"));
        }
        // Only the factory encodes a request and guards its body (D-082
        // N-2): a backend sends the body it is handed.
        if !matches!(relative.as_str(), "src/decision/outbound.rs" | "src/decision/wire.rs") {
            for name in ["check_outbound", "encode_request"] {
                if identifier_count(&compact, name) > 0 {
                    violations.push(format!("{relative}: {name}"));
                }
            }
        }
        // A concrete backend is built only by its transport.
        if relative != "src/decision/compatible_http.rs" && identifier_count(&compact, "CompatibleHttpBackend") > 0 {
            violations.push(format!("{relative}: CompatibleHttpBackend"));
        }
        // A transport is asked to build only by the factory, however it is
        // reached (`deps.transports.build(..)`), and only the factory fills
        // a backend spec (D-082 N-3).
        if compact.contains("transports.build(") {
            violations.push(format!("{relative}: builds through transports"));
        }
        let mut search = 0;
        while let Some(found) = compact[search..].find("BackendSpec {") {
            let start = search + found;
            search = start + "BackendSpec {".len();
            let before = compact[..start].trim_end();
            if !["struct", "impl", "for", "->"]
                .iter()
                .any(|token| before.ends_with(token))
            {
                violations.push(format!("{relative}: BackendSpec literal"));
            }
        }
        if identifier_count(&compact, "DecisionBackend") > 0 || identifier_count(&compact, "GuardedBody") > 0 {
            let calls = compact.matches(".evaluate(").count();
            let own = compact.matches("self.evaluate(").count();
            if calls > own {
                violations.push(format!("{relative}: evaluates a decision backend"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "only the decision backend factory validates, answers with the builtin, builds or evaluates a backend \
         (AG-04): {violations:?}"
    );
}

/// AG-17 (CDP-PLAN 1.5 and 1.7; SP-CDP-A 5.3 and 5.8): whether a decision
/// endpoint is loopback is decided in one place, `decision::loopback`, from
/// the host as written. Configuration validation, the factory's locality and
/// the HTTP client share it. Elsewhere in `src/decision/**` and in
/// `src/causal_tree/config.rs`:
///
/// - no parsed host is consulted (`host_str`, which normalizes `127.1` and
///   `0x7f000001` to `127.0.0.1`);
/// - no address type is built or matched (`Ipv4Addr`, `Ipv6Addr`), except by
///   the HTTP client, which maps `localhost` to `127.0.0.1` itself;
/// - `is_loopback` is used only by the compatible backend's check of the
///   peer it actually reached.
#[test]
pub(super) fn loopback_is_decided_only_by_the_shared_predicate() {
    const RULES: [(&str, &[&str]); 4] = [
        ("host_str", &["src/decision/loopback.rs"]),
        ("Ipv4Addr", &["src/decision/loopback.rs", "src/decision/http.rs"]),
        ("Ipv6Addr", &["src/decision/loopback.rs", "src/decision/http.rs"]),
        (
            "is_loopback",
            &["src/decision/loopback.rs", "src/decision/compatible_http.rs"],
        ),
    ];
    let mut violations = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if !relative.starts_with("src/decision/") && relative != "src/causal_tree/config.rs" {
            continue;
        }
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        for (name, allowed) in RULES {
            if !allowed.contains(&relative.as_str()) && contains_identifier(&production, name) {
                violations.push(format!("{relative}: {name}"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "loopback is decided only by decision::loopback (AG-17): {violations:?}"
    );
}

/// AG-21 (SP-CDP-A 5.8): every decision HTTP client comes from
/// `src/decision/http.rs`, which never follows a redirect, ignores every
/// proxy and resolves `localhost` itself for loopback, and is HTTPS only
/// for remote. No decision file builds another client or turns a
/// certificate check off.
#[test]
pub(super) fn decision_http_clients_come_only_from_the_pinned_builder() {
    const HTTP: &str = "src/decision/http.rs";
    const PINNED: [&str; 5] = [
        "redirect::Policy::none()",
        ".no_proxy()",
        ".https_only(true)",
        ".https_only(false)",
        ".resolve(",
    ];
    const OTHER_CLIENTS: [&str; 7] = [
        "ClientBuilder",
        "Client::builder",
        "Client::new",
        "Client::default",
        "reqwest::get(",
        "reqwest::blocking",
        "build_runtime_proxy_client",
    ];
    let mut violations = Vec::new();
    let http = strip_cfg_test_items(&blank_comments_and_literals(&read_source(
        &repository_root().join(HTTP),
    )));
    let compact: String = http.split_whitespace().collect();
    for call in PINNED {
        if !compact.contains(call) {
            violations.push(format!("{HTTP}: no {call}"));
        }
    }
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if !relative.starts_with("src/decision/") {
            continue;
        }
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        let compact: String = production.split_whitespace().collect();
        if compact.contains("danger_") || compact.contains("Policy::limited") || compact.contains("Policy::custom") {
            violations.push(format!("{relative}: disables a check or follows redirects"));
        }
        if relative != HTTP && OTHER_CLIENTS.iter().any(|name| compact.contains(name)) {
            violations.push(format!("{relative}: builds an HTTP client"));
        }
    }
    assert!(
        violations.is_empty(),
        "decision HTTP clients come only from the pinned builder (AG-21): {violations:?}"
    );
}

/// AG-10 (SP-CDP-A 5.10): the decision store's database is named, and so
/// opened, only by `causal_tree/store.rs`. Its `store/` child modules are
/// scanned like any other file; test-only files are empty production text.
/// Comments may mention it.
#[test]
pub(super) fn the_decision_store_database_is_opened_only_by_the_store() {
    const STORE: &str = "src/causal_tree/store.rs";
    const SCANNED_CHILDREN: [&str; 2] = [
        "src/causal_tree/store/child_outcome.rs",
        "src/causal_tree/store/child_recovery.rs",
    ];
    let mut violations = Vec::new();
    let mut scanned_children = 0_usize;
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if !relative.starts_with("src/") || relative == STORE {
            continue;
        }
        if SCANNED_CHILDREN.contains(&relative.as_str()) {
            scanned_children += 1;
        }
        let production = strip_cfg_test_items(&read_source(&path));
        let names_the_database = production.lines().any(|line| {
            let code = line.split_once("//").map_or(line, |(code, _)| code);
            code.contains("decisions.db")
        });
        if names_the_database {
            violations.push(relative);
        }
    }
    assert_eq!(
        scanned_children,
        SCANNED_CHILDREN.len(),
        "the store child modules must be scanned, not exempted"
    );
    assert!(
        violations.is_empty(),
        "only the decision store opens decisions.db (AG-10): {violations:?}"
    );
}

/// AG-11 (SP-CDP-A 5.11): the decision-plane metrics take their labels from
/// bounded allowlists only, on every metrics backend.
///
/// - Prometheus: every production `prx_causal_*` metric declares labels from
///   `CAUSAL_METRIC_LABELS`, and no production code of the file names the
///   lineage (session, owner, run and decision ids go to logs and trace
///   attributes only) or the configured model, so no helper can carry them
///   into a label.
/// - OpenTelemetry: the causal counter is recorded only with
///   `causal_metric_attributes`, which names neither.
#[test]
pub(super) fn decision_metric_labels_come_from_bounded_allowlists() {
    const CAUSAL_METRIC_LABELS: [&str; 7] = ["backend", "locality", "served_by", "error_class", "from", "to", "state"];
    const UNBOUNDED: [&str; 7] = [
        "lineage",
        "session_key",
        "owner_id",
        "run_id",
        "task_id",
        "decision_id",
        "configured_model",
    ];
    const PROMETHEUS: &str = "src/observability/prometheus.rs";
    const OTEL: &str = "src/observability/otel.rs";
    let raw = read_source(&repository_root().join(PROMETHEUS));
    // Production text with its literals kept (the names and labels are
    // literals), and the same text blanked for identifier checks.
    let production = strip_cfg_test_items(&raw);
    let mut violations = Vec::new();
    let mut declared = 0_usize;
    let mut search = 0;
    while let Some(found) = production[search..].find("\"prx_causal_") {
        let start = search + found;
        search = start + 1;
        declared += 1;
        let window = &production[start..production.len().min(start + 400)];
        let labels = window
            .find("&[")
            .filter(|open| !window[..*open].contains(".map_err"))
            .and_then(|open| {
                window[open + 2..]
                    .find(']')
                    .map(|close| &window[open + 2..open + 2 + close])
            });
        for label in labels.into_iter().flat_map(|list| list.split(',')) {
            let label = label.trim().trim_matches('"');
            if !label.is_empty() && !CAUSAL_METRIC_LABELS.contains(&label) {
                violations.push(format!("{PROMETHEUS}: label `{label}`"));
            }
        }
    }
    assert!(declared >= 6, "the decision metrics are declared in {PROMETHEUS}");
    let blanked = strip_cfg_test_items(&blank_comments_and_literals(&raw));
    assert!(blanked.contains("fn record_causal("), "record_causal exists");
    for field in UNBOUNDED {
        if contains_identifier(&blanked, field) {
            violations.push(format!("{PROMETHEUS}: names `{field}`"));
        }
    }
    let otel = strip_cfg_test_items(&blank_comments_and_literals(&read_source(
        &repository_root().join(OTEL),
    )));
    let compact: String = otel.split_whitespace().collect();
    let mut adds = 0_usize;
    let mut search = 0;
    while let Some(found) = compact[search..].find("causal_events.add(") {
        let start = search + found;
        search = start + 1;
        adds += 1;
        if !compact[start..].starts_with("causal_events.add(1,&causal_metric_attributes(") {
            violations.push(format!("{OTEL}: the causal counter is recorded with other attributes"));
        }
    }
    assert!(adds >= 1, "the causal counter is recorded in {OTEL}");
    match otel.find("fn causal_metric_attributes(") {
        Some(start) => {
            let rest = &otel[start..];
            let body = rest.find("\n}\n").map_or(rest, |end| &rest[..end]);
            for field in UNBOUNDED {
                if contains_identifier(body, field) {
                    violations.push(format!("{OTEL}: causal_metric_attributes reads `{field}`"));
                }
            }
        }
        None => violations.push(format!("{OTEL}: no causal_metric_attributes")),
    }
    assert!(
        violations.is_empty(),
        "decision metric labels come from bounded allowlists (AG-11): {violations:?}"
    );
}
