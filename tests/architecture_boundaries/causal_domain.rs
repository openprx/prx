use super::helpers::{
    blank_comments_and_literals, normalize_line, read_source, relative_path, repository_root, rust_source_files,
    strip_cfg_test_items,
};
use super::tool_scanner::contains_identifier;

/// AG-05. `MockDecisionBackend` is compiled only for tests: its module is
/// declared behind `cfg(any(test, feature = "test-mock"))`, and no production
/// code outside that module names it.
#[test]
pub(super) fn mock_decision_backend_is_compiled_only_for_tests() {
    let mut sites = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if relative == "src/decision/mock.rs" {
            continue;
        }
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        for needle in ["MockDecisionBackend", "MockOutcome", "MockStep"] {
            if contains_identifier(&production, needle) {
                sites.push(format!("{relative}: {needle}"));
            }
        }
        if production.contains("decision::mock") {
            sites.push(format!("{relative}: decision::mock"));
        }
    }
    assert!(
        sites.is_empty(),
        "the scripted decision backend is test-only (AG-05); production code names it at {sites:?}"
    );
    let module = read_source(&repository_root().join("src/decision/mod.rs"));
    let lines: Vec<String> = module.lines().map(normalize_line).collect();
    let declarations: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.ends_with("mod mock;"))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        declarations.len(),
        1,
        "src/decision/mod.rs declares `mod mock` exactly once"
    );
    assert!(
        declarations[0] > 0 && lines[declarations[0] - 1] == "#[cfg(any(test, feature = \"test-mock\"))]",
        "`mod mock` must sit directly behind `#[cfg(any(test, feature = \"test-mock\"))]`"
    );
}

/// The types that carry text a decision backend returned.
const DECISION_RESPONSE_TYPES: [&str; 6] = [
    "DecisionResponse",
    "DecisionAnswer",
    "ChoiceAnswer",
    "ScoreAnswer",
    "NoulAnswer",
    "ValidatedDecision",
];

/// AG-15 (invariant 16.13). No backend-returned text becomes a chat message:
/// the decision module never names `ChatMessage`, no production file that
/// handles a decision response constructs a chat message, and the wire codec
/// declares no field for free-form backend text, so an `explanation`,
/// `reason` or `reasoning` field is never decoded.
///
/// A textual guard: a message built from a response in a file that names
/// none of the response types (through a helper elsewhere) is not found.
#[test]
pub(super) fn decision_answers_never_become_chat_messages() {
    const MESSAGE_CONSTRUCTORS: [&str; 4] = [
        "ChatMessage::system(",
        "ChatMessage::user(",
        "ChatMessage::assistant(",
        "ChatMessage {",
    ];
    let mut violations = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        if relative.starts_with("src/decision/") {
            if contains_identifier(&production, "ChatMessage") {
                violations.push(format!("{relative}: the decision module names ChatMessage"));
            }
            continue;
        }
        let handles_responses = DECISION_RESPONSE_TYPES
            .iter()
            .any(|name| contains_identifier(&production, name));
        let compact = production.split_whitespace().collect::<Vec<_>>().join(" ");
        if handles_responses {
            for constructor in MESSAGE_CONSTRUCTORS {
                if compact.contains(constructor) {
                    violations.push(format!("{relative}: builds `{constructor}` beside a decision response"));
                }
            }
        }
    }
    let wire = strip_cfg_test_items(&blank_comments_and_literals(&read_source(
        &repository_root().join("src/decision/wire.rs"),
    )));
    for field in ["explanation", "reason", "reasoning", "rationale"] {
        if contains_identifier(&wire, field) {
            violations.push(format!("src/decision/wire.rs: declares or reads `{field}`"));
        }
    }
    assert!(
        violations.is_empty(),
        "backend-returned text must never reach a chat message (AG-15): {violations:?}"
    );
}

/// AG-22. The path from backend bytes to a validated decision never repairs
/// a value: no normalizing, rescaling or clamping call anywhere in
/// `src/decision/**` (D-057 L-7: every decision file, not only the codec and
/// the validator). The property corpus in
/// `tests/proptest_decision_validate.rs` proves the bits are kept.
///
/// The scan is textual, so a helper outside `src/decision/` escapes it. That
/// is allowed only for code that *classifies request text*, never for code
/// that changes a decision value (D-061 amendment 2). The one such helper,
/// `crate::util::is_combining_mark`, reads a Unicode character class for the
/// builtin's structure scan. A wrapper that normalizes, rescales, clamps or
/// repairs a probability, level or answer anywhere is a violation of this
/// guard, whatever file it lives in, and cannot be accepted by analogy.
#[test]
pub(super) fn the_decision_validator_never_repairs_a_value() {
    const REPAIRS: [&str; 5] = ["normaliz", "normalis", "rescale", "clamp(", "repair"];
    let mut violations = Vec::new();
    let mut scanned = 0_usize;
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if !relative.starts_with("src/decision/") {
            continue;
        }
        scanned += 1;
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path))).to_ascii_lowercase();
        for repair in REPAIRS {
            if production.contains(repair) {
                violations.push(format!("{relative}: {repair}"));
            }
        }
    }
    assert!(scanned >= 7, "the decision module has at least its seven CDP-01 files");
    assert!(
        violations.is_empty(),
        "decision responses are validated, never repaired (AG-22): {violations:?}"
    );
}

/// The paths into the rest of the crate that the causal domain may name, as
/// `crate::` prefixes (AG-01). Everything else is out of reach: a decision
/// backend (`decision::compatible_http`, `CompatibleHttpBackend`, CDP-07's
/// `decision::http`), a chat provider, a client builder, a runtime service.
/// The one redactor is `security::redaction` (D-054, D-064).
const CAUSAL_DOMAIN_CRATE_PATHS: &[&str] = &[
    "causal_tree::",
    // The configuration names a backend kind and checks an endpoint's
    // spelling (CDP-03); neither opens a connection.
    "decision::BackendKind",
    "decision::loopback::",
    "decision::model_pin::is_unpinned_model",
    "decision::CandidateKey",
    "decision::DecisionAnswer",
    "decision::DecisionError",
    "decision::DecisionKey",
    "decision::DecisionRequest",
    "decision::KeyDomain",
    // The credential reference grammar (CDP-12): a pure check of the id.
    "config::manage::credentials::CredentialReferences::reference",
    // The store's directory passes the key's directory checks (D-091 F-1).
    "decision::key::check_decision_dir",
    "decision::key::prepare_decision_dir",
    "decision::QuestionId",
    "decision::StructuredValue",
    "decision::ValidatedDecision",
    "decision::builtin::weights::FRESH_ARTIFACT_MS",
    "decision::builtin::weights::STALE_ARTIFACT_MS",
    "observability::Observer",
    "observability::ObserverEvent",
    "observability::ObserverMetric",
    // CDP-09..11 (store, evaluation, admin): the decision store reports
    // through the causal event sink and uses the bounded busy handler; the
    // admin service reads the configuration and the factory the caller
    // builds (its `DecisionPlaneBuilder`: the transports and the catalog
    // stay outside the domain), asks it the neutral fixture, and shows
    // provider labels redacted. None of these opens a connection.
    "config::Config",
    "decision::telemetry::CausalEvent",
    "decision::telemetry::CausalEventFields",
    "decision::telemetry::CausalEventKind",
    "decision::telemetry::CausalEventSink",
    "decision::telemetry::CausalLineage",
    "decision::telemetry::IncompleteReason",
    "decision::factory::AttemptOutcome",
    // The one reading of the configured backend (CDP-14's live-test target).
    "decision::factory::configured_selection",
    "decision::factory::DecisionBackendFactory",
    "decision::factory::FactoryBuildError",
    "decision::factory::FactoryDeps",
    "decision::factory::HealthView",
    "decision::factory::ServedBy",
    "decision::BackendLocality",
    "decision::DecisionQuestion",
    "decision::GlobalQuestion",
    "decision::OptionId",
    "decision::RequestLimits",
    "decision::RequestParts",
    "decision::builtin::weights::MODEL_VERSION",
    "providers::redact_endpoint_userinfo",
    "runtime::sqlite_pool::install_bounded_busy_handler",
    "security::redaction::redact_json_value",
    "security::redaction::redact_secrets",
];

// CDP-18's admission and enumeration ports read immutable catalog and policy
// snapshots. Keep these imports local to those two files: state, projection,
// and backend-free domain types retain the narrower AG-01 allowlist.
const CAUSAL_GATE_CRATE_PATHS: &[&str] = &[
    "capability::CapabilityAvailabilityLevel",
    "security::AutonomyLevel",
    "security::SecurityPolicy",
    "tools::capability::catalog::CapabilityEntry",
    "tools::capability::catalog::CatalogView",
    "tools::capability::catalog::Exposure",
    "tools::capability::child::CapabilityConstraints",
    "tools::capability::child::ChildCapabilityManifest",
    "tools::execution::ToolExecutionDecision",
    "tools::execution::decide_tool_execution",
];

const CAUSAL_GENERATOR_CRATE_PATHS: &[&str] = &[
    "tools::capability::catalog::CapabilityEntry",
    "tools::capability::catalog::CatalogView",
    "tools::capability::catalog::Exposure",
];

// CDP-19 constructs a sealed, provider-neutral request from typed protocol
// values. This allowance belongs only to the question compiler.
const CAUSAL_QUESTION_CRATE_PATHS: &[&str] = &[
    "decision::CandidateQuestion",
    "decision::DecisionError",
    "decision::DecisionKey",
    "decision::DecisionQuestion",
    "decision::DecisionRequest",
    "decision::GlobalQuestion",
    "decision::OmissionReason",
    "decision::OptionId",
    "decision::ProjectionAudit",
    "decision::QuestionId",
    "decision::RequestLimits",
    "decision::RequestParts",
    "decision::RequestRule",
    "decision::StructuredValue",
];

// CDP-20's prediction reader stays protocol-only; it cannot import a
// backend, transport or live catalog.
const CAUSAL_SCORE_CRATE_PATHS: &[&str] = &[
    "decision::CandidateQuestion",
    "decision::OmissionReason",
    "decision::ScoreAnswer",
];

// The bounded rehearsal sees only the pinned catalog snapshot.
const CAUSAL_REHEARSAL_CRATE_PATHS: &[&str] = &["tools::capability::catalog::CatalogView"];

// The engine is the one causal-domain caller of the factory. Its selection
// inputs are a validated result and an immutable catalog view, not a client.
const CAUSAL_ENGINE_CRATE_PATHS: &[&str] = &[
    "capability::CapabilityAvailabilityLevel",
    "decision::factory::PredictionResult",
    "security::AutonomyLevel",
    "security::SecurityPolicy",
    "tools::capability::catalog::CatalogView",
    "tools::capability::catalog::Exposure",
    "tools::capability::child::CapabilityConstraints",
    "tools::capability::child::ChildCapabilityManifest",
    "tools::capability::child::ChildManifestError",
    "tools::execution::ToolExecutionDecision",
    "tools::execution::decide_tool_execution",
    "tools::capability::search::CatalogSearch",
];

// Outcome records retain typed contracts without performing I/O. Only this
// file may name these dependencies; execution and service construction stay closed.
const CAUSAL_OUTCOME_CRATE_PATHS: &[&str] = &[
    "agent::terminal::VerifiedTerminalReplay",
    "tools::capability::execution_trace::ExecutionTraceSnapshot",
    "runtime::turn_source::ingress_event_surface",
    "runtime::turn_source::subject_task_id",
    "agent::terminal::TurnTerminalReceipt",
    "agent::terminal::TurnTerminalStatus",
    "cost::types::CostSettlement",
    "cost::tracker::ceil_micro_usd",
    "decision::factory::FallbackReason",
    "decision::PROTOCOL_VERSION",
    "llm::route_decision::AttemptStatus",
    "llm::route_decision::ExecutionStatus",
    "llm::route_decision::MeteredTokenUsageRecord",
    "llm::route_decision::ProviderExecutionOutcome",
    "memory::MessageEvent",
    "memory::MessageEventSource::Agent",
    "runtime::turn_decision::TurnDecisionContext",
];

// The directive carries a sealed child capability manifest to the runtime;
// only this conversion module may name the manifest and its constraints.
const CAUSAL_DIRECTIVE_CRATE_PATHS: &[&str] = &["tools::capability::child::ChildCapabilityManifest"];

/// AG-24 and AG-26: decisions may use an absolute deadline and may describe
/// an approval scope, but they cannot create a nested duration timeout or an
/// authenticated approval response/grant. Test-only fixtures are excluded.
#[test]
pub(super) fn decision_domain_has_no_constant_timeout_or_approval_grant_constructor() {
    let mut violations = Vec::new();
    let mut scanned = 0_usize;
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if !relative.starts_with("src/decision/") && !relative.starts_with("src/causal_tree/") {
            continue;
        }
        scanned += 1;
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        for forbidden in [
            "tokio::time::timeout(",
            "ApprovalResponse",
            "ApprovalGrantV2",
            "issue_one_shot(",
            "sign_grant(",
        ] {
            if production.contains(forbidden) {
                violations.push(format!("{relative}: {forbidden}"));
            }
        }
    }
    assert!(scanned > 0, "decision-domain guard scanned no production source");
    assert!(
        violations.is_empty(),
        "decision-domain boundary violations: {violations:?}"
    );
}

/// Names the causal domain must never use, however it reaches them: network
/// clients and sockets, the proxy-aware client builders and the chat
/// provider factory. (The chat `Provider` trait lives under
/// `crate::providers`, which the path allowlist already refuses; the name
/// itself is also an observed step kind, so it is not banned as a word.)
const CAUSAL_DOMAIN_FORBIDDEN_NAMES: [&str; 25] = [
    "reqwest",
    "hyper",
    "ureq",
    "isahc",
    "curl",
    "TcpStream",
    "TcpListener",
    "TcpSocket",
    "UdpSocket",
    "UnixStream",
    "UnixListener",
    "UnixDatagram",
    "ToSocketAddrs",
    "lookup_host",
    // Processes (a `curl` subprocess is a network client too).
    "process",
    "Command",
    // Websocket, MCP and mail clients.
    "tungstenite",
    "tokio_tungstenite",
    "connect_async",
    "rmcp",
    "lettre",
    "build_runtime_proxy_client",
    "build_runtime_proxy_client_with_timeouts",
    "apply_runtime_proxy_to_builder",
    "create_provider",
];

/// Where a crate-rooted path in a causal file starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PathRoot {
    /// `crate::`, or `super::` in `src/causal_tree/mod.rs`, where it is the
    /// crate root.
    Crate,
    /// `super::super::`: the crate root from a top-level causal file, or the
    /// causal domain itself from a module nested in one.
    Relative,
}

/// Every path a `use` tree or a qualified path names from the crate root,
/// with its root, and with brace groups expanded: `crate::decision::{A, b::C}`
/// gives `decision::A` and `decision::b::C`; a glob gives `...::*`. In
/// `mod.rs`, `super::` is the crate root too (D-066 N-5).
pub(super) fn crate_rooted_paths(production: &str, is_mod_rs: bool) -> Vec<(PathRoot, String)> {
    let mut paths = Vec::new();
    let mut roots = vec![("crate::", PathRoot::Crate), ("super::super::", PathRoot::Relative)];
    if is_mod_rs {
        roots.push(("super::", PathRoot::Crate));
    }
    for (root, kind) in roots {
        let mut search = 0;
        while let Some(found) = production[search..].find(root) {
            let start = search + found;
            search = start + root.len();
            let before = production[..start].chars().next_back();
            if before.is_some_and(|character| character.is_alphanumeric() || character == '_' || character == ':') {
                continue;
            }
            if root == "super::" && production[search..].starts_with("super::") {
                continue;
            }
            let mut found_paths = Vec::new();
            use_tree_paths(production, search, "", &mut found_paths);
            paths.extend(found_paths.into_iter().map(|path| (kind, path)));
        }
    }
    paths
}

/// The paths of the use tree that starts at byte `at`, each prefixed with
/// `prefix`; returns where the tree ends.
fn use_tree_paths(text: &str, at: usize, prefix: &str, paths: &mut Vec<String>) -> usize {
    let bytes = text.as_bytes();
    let skip_space = |mut index: usize| {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        index
    };
    let mut index = skip_space(at);
    if index < bytes.len() && bytes[index] == b'{' {
        index += 1;
        loop {
            index = skip_space(index);
            if index >= bytes.len() {
                return index;
            }
            match bytes[index] {
                b'}' => return index + 1,
                b',' => index += 1,
                _ => {
                    let end = use_tree_paths(text, index, prefix, paths);
                    index = end.max(index + 1);
                }
            }
        }
    }
    if index < bytes.len() && bytes[index] == b'*' {
        paths.push(format!("{prefix}*"));
        return index + 1;
    }
    let start = index;
    while index < bytes.len() && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_') {
        index += 1;
    }
    let segment = &text[start..index];
    if segment.is_empty() {
        paths.push(prefix.to_string());
        return index;
    }
    let path = format!("{prefix}{segment}");
    let after = skip_space(index);
    if text[after..].starts_with("::") {
        return use_tree_paths(text, after + 2, &format!("{path}::"), paths);
    }
    paths.push(path);
    index
}

/// AG-01 (I-04.3; D-064 M-5). The causal domain is pure: building,
/// projecting and fingerprinting a turn state never reaches the network. It
/// reaches a backend only through `DecisionBackend`, from the caller. The
/// guard is an allowlist, so a new route out cannot pass by being unnamed:
///
/// - every path a production file under `src/causal_tree/**` names from the
///   crate root (`crate::`, or `super::super::` from a top-level file,
///   through `use` trees, aliases and qualified paths alike) must start with
///   an entry of [`CAUSAL_DOMAIN_CRATE_PATHS`]; a glob or a bare module
///   (`use crate::decision as d`) is refused;
/// - none of [`CAUSAL_DOMAIN_FORBIDDEN_NAMES`] appears as an identifier;
/// - no `#[path]` attribute or `include!` pulls another file in.
#[test]
pub(super) fn the_causal_domain_imports_no_backend_and_no_network() {
    let mut violations = Vec::new();
    let mut scanned = 0_usize;
    let causal_modules: Vec<String> = rust_source_files()
        .iter()
        .map(|path| relative_path(path))
        .filter_map(|relative| {
            relative
                .strip_prefix("src/causal_tree/")
                .and_then(|file| file.strip_suffix(".rs"))
                .filter(|stem| !stem.contains('/') && *stem != "mod")
                .map(str::to_string)
        })
        .collect();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if !relative.starts_with("src/causal_tree/") {
            continue;
        }
        scanned += 1;
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        for (root, named) in crate_rooted_paths(&production, relative == "src/causal_tree/mod.rs") {
            // From a module nested inside a causal file (a test module under
            // `cfg(all(test, ..))`, which the scan keeps), `super::super::`
            // is the causal domain itself, not the crate root. Only such a
            // relative path is read that way; `crate::<name>` never is
            // (D-066 N-5).
            if root == PathRoot::Relative
                && causal_modules
                    .iter()
                    .any(|module| named == *module || named.starts_with(&format!("{module}::")))
            {
                continue;
            }
            let file_paths = match relative.as_str() {
                "src/causal_tree/gates.rs" => CAUSAL_GATE_CRATE_PATHS,
                "src/causal_tree/candidate_generator.rs" => CAUSAL_GENERATOR_CRATE_PATHS,
                "src/causal_tree/questions.rs" => CAUSAL_QUESTION_CRATE_PATHS,
                "src/causal_tree/decision_score.rs" => CAUSAL_SCORE_CRATE_PATHS,
                "src/causal_tree/decision_rehearsal.rs" => CAUSAL_REHEARSAL_CRATE_PATHS,
                "src/causal_tree/decision_engine.rs" => CAUSAL_ENGINE_CRATE_PATHS,
                "src/causal_tree/directive.rs" => CAUSAL_DIRECTIVE_CRATE_PATHS,
                "src/causal_tree/outcome.rs" => CAUSAL_OUTCOME_CRATE_PATHS,
                "src/causal_tree/admin.rs" => &[
                    "config::files::with_consistent_config_snapshot",
                    "config::files::read_merged_toml_with_gate_once",
                    "config::deprecations::file_holding",
                ],
                "src/causal_tree/store/child_outcome.rs" => &["agent::terminal::VerifiedTerminalReplay"],
                _ => &[],
            };
            let allowed = CAUSAL_DOMAIN_CRATE_PATHS.iter().chain(file_paths.iter()).any(|entry| {
                if entry.ends_with("::") {
                    named.starts_with(entry)
                } else {
                    named == *entry || named.starts_with(&format!("{entry}::"))
                }
            });
            if !allowed {
                violations.push(format!("{relative}: crate::{named}"));
            }
        }
        for name in CAUSAL_DOMAIN_FORBIDDEN_NAMES {
            if contains_identifier(&production, name) {
                violations.push(format!("{relative}: {name}"));
            }
        }
        let compact: String = production.split_whitespace().collect();
        for inclusion in ["#[path", "include!("] {
            if compact.contains(inclusion) {
                violations.push(format!("{relative}: {inclusion}"));
            }
        }
    }
    assert!(
        scanned >= 4,
        "the causal domain has at least mod, branch, state and projection"
    );
    assert!(
        violations.is_empty(),
        "the causal domain imports no backend and no network client (AG-01): {violations:?}"
    );
}
