use super::helpers;

#[test]
fn token_counting_is_not_feature_gated() {
    let forbidden = ["llm", "-router"].concat();
    for relative in [
        "src/agent/context_budget.rs",
        "src/agent/context_budget/token_timing.rs",
        "src/agent/turn_tests/context_budget.rs",
        "src/runtime/decision_accounting/token_preflight_tests.rs",
    ] {
        let source = helpers::read_source(&helpers::repository_root().join(relative));
        assert!(
            !source.contains(&forbidden),
            "{relative} must count independently of optional routing"
        );
    }
    let source = helpers::read_source(&helpers::repository_root().join("src/runtime/decision_accounting.rs"));
    assert!(
        !helpers::function_body(&source, "initialize_tokenizers").contains(&forbidden),
        "initialize_tokenizers must initialize counting unconditionally"
    );
    assert!(
        !helpers::impl_method_body(&source, "impl PreparedInput", "new").contains(&forbidden),
        "PreparedInput::new must retain the prefix bound unconditionally"
    );
    let declaration = source.find("mod cl100k_prefix_bound;").expect("prefix module");
    let attribute = source[..declaration]
        .trim_end()
        .lines()
        .next_back()
        .expect("module attribute or previous item");
    assert!(
        !attribute.contains(&forbidden),
        "cl100k_prefix_bound module must not have a routing attribute: {attribute}"
    );
}

#[test]
fn context_window_resolution_lives_in_providers() {
    let needles = [
        ["router", "::context"].concat(),
        ["router", "::resolve_effective_compaction_config"].concat(),
    ];
    let mut violations = Vec::new();
    for path in helpers::rust_source_and_test_files() {
        let relative = helpers::relative_path(&path);
        if relative.starts_with("src/router/") || relative == "tests/architecture_boundaries/token_count.rs" {
            continue;
        }
        for (index, line) in helpers::read_source(&path).lines().enumerate() {
            if needles.iter().any(|needle| line.contains(needle)) {
                violations.push(format!("{relative}:{}", index + 1));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "context-window callers must use providers: {}",
        violations.join(", ")
    );
}
