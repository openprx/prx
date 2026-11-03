//! Guards the prompt-only evolution surface and operator migration contract.

#[test]
fn removed_layers_have_no_files_or_memory_trash_calls() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let evolution = root.join("src/self_system/evolution");
    for name in [
        "strategy_evolution.rs",
        "memory_evolution.rs",
        "memory_compressor.rs",
        "memory_retrieval.rs",
    ] {
        assert!(!evolution.join(name).exists(), "retired evolution file exists: {name}");
    }
    for file in std::fs::read_dir(evolution).unwrap() {
        let path = file.unwrap().path();
        if path.is_file() {
            assert!(
                !std::fs::read_to_string(&path).unwrap().contains("move_to_trash"),
                "memory trash call in {}",
                path.display()
            );
        }
    }
}

#[test]
fn operator_docs_describe_only_lifecycle_evolution() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let docs = std::fs::read_to_string(root.join("docs/evolution.md")).unwrap();
    for removed in ["decision_policy", "retrieval_fusion"] {
        assert!(!docs.contains(removed), "obsolete documentation: {removed}");
    }
    let changelog = std::fs::read_to_string(root.join("CHANGELOG.md")).unwrap();
    let unreleased = changelog
        .split("## [Unreleased]")
        .nth(1)
        .unwrap()
        .split("\n## ")
        .next()
        .unwrap();
    assert!(unreleased.contains("BREAKING (operators)"));
    assert!(unreleased.contains("skills lifecycle"));
}
