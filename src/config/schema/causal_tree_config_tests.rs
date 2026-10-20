#![cfg(test)]

use super::*;

/// The exact `[causal_tree]` block a `Config::save` wrote at `8a5d160d`.
const SAVED_BY_8A5D160D: &str = r#"
default_temperature = 0.7

[causal_tree]
enabled = false
w_confidence = 0.5
w_cost = 0.25
w_latency = 0.25
write_decision_log = true
write_metrics = true

[causal_tree.policy]
max_branches = 3
commit_threshold = 0.6200000047683716
extra_token_ratio_limit = 0.3499999940395355
extra_latency_budget_ms = 300
rehearsal_timeout_ms = 5000
default_side_effect_mode = "ReadOnly"
circuit_breaker_threshold = 5
circuit_breaker_cooldown_secs = 60
"#;

fn load(text: &str) -> (tempfile::TempDir, Result<Config>) {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, text).expect("test: write config");
    let loaded = Config::load_from_path(&path, dir.path().join("workspace"));
    (dir, loaded)
}

fn saved_text(dir: &Path) -> String {
    let mut text = std::fs::read_to_string(dir.join("config.toml")).expect("test: main file");
    if let Ok(entries) = std::fs::read_dir(dir.join("config.d")) {
        for entry in entries {
            let path = entry.expect("test: entry").path();
            text.push_str(&std::fs::read_to_string(path).expect("test: fragment"));
        }
    }
    text
}

#[test]
fn a_backend_table_loads() {
    let (_dir, loaded) = load(
        "default_temperature = 0.7\n\n[causal_tree]\nenabled = true\n\n[causal_tree.backend]\nkind = \"auto\"\n\n\
             [causal_tree.backend.circuit_breaker]\nfailure_threshold = 7\n\n[causal_tree.rehearsal]\ntimeout_ms = 80\n",
    );
    let loaded = loaded.expect("test: loads");
    assert!(loaded.causal_tree.enabled);
    assert_eq!(
        loaded.causal_tree.backend.kind,
        crate::causal_tree::config::BackendSelection::Auto
    );
    assert_eq!(
        loaded.causal_tree.backend.circuit_breaker.failure_threshold, 7,
        "the configured threshold survives load"
    );
    assert_eq!(loaded.causal_tree.rehearsal.timeout_ms, 80);
}

#[test]
fn a_config_saved_by_8a5d160d_loads_and_maps() {
    let (_dir, loaded) = load(SAVED_BY_8A5D160D);
    let loaded = loaded.expect("test: the legacy save loads");
    assert_eq!(loaded.causal_tree, crate::causal_tree::CausalTreeConfig::default());
    assert!(
        loaded.causal_tree.enabled,
        "CDP-31 enables the local decision plane by default"
    );
}

/// V8.06: upgraded configs keep operator values.
#[test]
fn legacy_non_default_values_move_to_their_new_home() {
    let text = SAVED_BY_8A5D160D
        .replace("max_branches = 3", "max_branches = 5")
        .replace("circuit_breaker_threshold = 5", "circuit_breaker_threshold = 9")
        .replace("w_cost = 0.25", "w_cost = 0.4000000059604645");
    let (_dir, loaded) = load(&text);
    let causal = loaded.expect("test: loads").causal_tree;
    assert_eq!(causal.policy.max_candidates, 5);
    assert_eq!(causal.backend.circuit_breaker.failure_threshold, 9);
    assert_eq!(causal.scoring.weights.w_cost, Some(0.400_000_005_960_464_5));
}

#[test]
fn legacy_w_confidence_non_default_fails_with_a_precise_error() {
    let (dir, loaded) = load(&SAVED_BY_8A5D160D.replace("w_confidence = 0.5", "w_confidence = 0.7123"));
    let message = format!("{:#}", loaded.expect_err("test: refused"));
    assert!(
        message.contains(&dir.path().join("config.toml").display().to_string()),
        "{message}"
    );
    assert!(message.contains("causal_tree.w_confidence"), "{message}");
    assert!(message.contains("[causal_tree.scoring.weights]"), "{message}");
    assert!(!message.contains("0.7123"), "{message}");
}

/// A naive move of the legacy 5000 ms rehearsal timeout would fail the
/// budget rule on every upgraded config.
#[test]
fn a_legacy_rehearsal_timeout_default_does_not_fail_the_budget_rule() {
    let (_dir, loaded) = load(SAVED_BY_8A5D160D);
    let causal = loaded.expect("test: loads").causal_tree;
    assert_eq!(causal.rehearsal.timeout_ms, 100);
    assert_eq!(causal.policy.extra_latency_budget_ms, 500);

    let (_dir, loaded) = load(&SAVED_BY_8A5D160D.replace("rehearsal_timeout_ms = 5000", "rehearsal_timeout_ms = 900"));
    let message = format!("{:#}", loaded.expect_err("test: over the budget"));
    assert!(
        message.contains("causal_tree.rehearsal.timeout_ms must be at most"),
        "{message}"
    );
}

#[test]
fn an_invalid_backend_stops_the_load_without_echoing_the_endpoint() {
    let (_dir, loaded) = load(
        "default_temperature = 0.7\n\n[causal_tree.backend]\nkind = \"compatible_http\"\n\
             endpoint = \"https://decider:hunter2pw@decisions.example.com/v1\"\n",
    );
    let message = format!("{:#}", loaded.expect_err("test: refused"));
    assert!(message.contains("causal_tree.backend.endpoint"), "{message}");
    assert!(!message.contains("hunter2pw"), "{message}");

    // A hosted service loads only with one exact model version (CDP-13).
    let (_dir, loaded) =
        load("default_temperature = 0.7\n\n[causal_tree.backend]\nkind = \"typesafe\"\nmodel = \"jev-latest\"\n");
    let message = format!("{:#}", loaded.expect_err("test: refused"));
    assert!(message.contains("hosted decision service"), "{message}");
    let (_dir, loaded) =
        load("default_temperature = 0.7\n\n[causal_tree.backend]\nkind = \"typesafe\"\nmodel = \"jev-1.13.0\"\n");
    loaded.expect("test: a pinned hosted model loads");
}

/// The decision log and metrics switches never had an effect; a versioned
/// file that stores them still loads, and a save does not write them back.
#[tokio::test]
async fn stored_observability_switches_load_and_are_not_written_back() {
    let (dir, loaded) = load(
        "default_temperature = 0.7\n\n[causal_tree]\nschema_version = 1\nenabled = true\n\n\
             [causal_tree.observability]\ndecision_log = false\nmetrics = false\n",
    );
    let loaded = loaded.expect("test: a stored observability table still loads");
    assert!(loaded.causal_tree.enabled);
    crate::config::files::write_split_config(&loaded, false)
        .await
        .expect("test: save");
    let saved = saved_text(dir.path());
    assert!(!saved.contains("decision_log") && !saved.contains("metrics"), "{saved}");
}

#[tokio::test]
async fn migration_is_idempotent_and_save_writes_no_legacy_key() {
    let text = SAVED_BY_8A5D160D.replace("max_branches = 3", "max_branches = 5");
    let (dir, loaded) = load(&text);
    let loaded = loaded.expect("test: loads");
    crate::config::files::write_split_config(&loaded, false)
        .await
        .expect("test: save");
    let saved = saved_text(dir.path());
    for legacy in [
        "w_confidence",
        "w_cost",
        "w_latency",
        "write_decision_log",
        "write_metrics",
        "max_branches",
        "rehearsal_timeout_ms",
        "default_side_effect_mode",
        "circuit_breaker_threshold",
        "circuit_breaker_cooldown_secs",
    ] {
        assert!(!saved.contains(legacy), "{legacy} in {saved}");
    }
    assert!(saved.contains("max_candidates = 5"), "{saved}");
    assert!(saved.contains("enabled = true"), "`enabled` is always written: {saved}");

    assert!(
        saved.contains("schema_version = 2"),
        "the version is always written: {saved}"
    );

    let reloaded =
        Config::load_from_path(&dir.path().join("config.toml"), dir.path().join("workspace")).expect("test: reload");
    assert_eq!(reloaded.causal_tree, loaded.causal_tree);
}

/// The BASE split layout keeps `[causal_tree]` in `config.d/agent.toml`
/// (P2-1): writes `files` under a temp dir and loads it.
fn load_split(files: &[(&str, &str)]) -> (tempfile::TempDir, Result<Config>) {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    std::fs::create_dir_all(dir.path().join("config.d")).expect("test: config.d");
    for (name, text) in files {
        std::fs::write(dir.path().join(name), text).expect("test: write");
    }
    let loaded = Config::load_from_path(&dir.path().join("config.toml"), dir.path().join("workspace"));
    (dir, loaded)
}

/// D-074: a migration error names the file that holds the key, here the
/// `config.d/agent.toml` fragment of the BASE layout, and a legacy key
/// split from its replacement names both files.
#[test]
fn a_migration_error_names_the_fragment_that_holds_the_key() {
    let block = SAVED_BY_8A5D160D
        .replace("default_temperature = 0.7\n", "")
        .replace("w_confidence = 0.5", "w_confidence = 0.7123");
    let (dir, loaded) = load_split(&[
        ("config.toml", "default_temperature = 0.7\n"),
        ("config.d/agent.toml", &block),
    ]);
    let message = format!("{:#}", loaded.expect_err("test: refused"));
    let agent = dir.path().join("config.d").join("agent.toml").display().to_string();
    assert!(
        message.contains(&format!("{agent}: causal_tree.w_confidence")),
        "{message}"
    );
    assert!(!message.contains("0.7123"), "{message}");

    let (dir, loaded) = load_split(&[
        (
            "config.toml",
            "default_temperature = 0.7\n\n[causal_tree.policy]\nmax_candidates = 6\n",
        ),
        ("config.d/agent.toml", "[causal_tree.policy]\nmax_branches = 5\n"),
    ]);
    let message = format!("{:#}", loaded.expect_err("test: refused"));
    let agent = dir.path().join("config.d").join("agent.toml").display().to_string();
    let main = dir.path().join("config.toml").display().to_string();
    assert!(
        message.contains(&format!(
            "{agent}: causal_tree.policy.max_branches is set together with"
        )) && message.contains(&format!("causal_tree.policy.max_candidates (in {main})")),
        "{message}"
    );
}

/// D-074 with X-3: the `[causal_tree]` block of the BASE `prx init` full
/// template (a bare `enabled = false` in `config.d/agent.toml`) is the
/// old serialized default and reads as unset; the first save writes the
/// versioned form, after which it is the operator's.
#[tokio::test]
async fn a_base_init_install_reads_its_disable_as_unset_until_saved() {
    const BASE_INIT_AGENT_BLOCK: &str = "# Causal tree (CTE) — experimental, opt-in. When enabled, the agent loop runs\n\
             # speculative multi-branch prediction (in-memory rehearsal, no extra LLM calls)\n\
             # before each turn. Default off; runtime evidence appears as observer CteRun events.\n\
             [causal_tree]\n\
             enabled = false\n";
    let (dir, loaded) = load_split(&[
        ("config.toml", "default_temperature = 0.7\n"),
        ("config.d/agent.toml", BASE_INIT_AGENT_BLOCK),
    ]);
    let loaded = loaded.expect("test: loads");
    assert_eq!(loaded.causal_tree, crate::causal_tree::CausalTreeConfig::default());
    let mut root: toml::Value = toml::from_str(BASE_INIT_AGENT_BLOCK).expect("test: toml");
    let report = crate::causal_tree::config::migrate_legacy(&mut root, &|_| None).expect("test: migrates");
    assert_eq!(report.dropped, ["enabled"], "the template's disable is the old default");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("config.d/agent.toml")).expect("test: read"),
        BASE_INIT_AGENT_BLOCK,
        "loading never stamps the marker on a file it only mapped in memory"
    );
    crate::config::files::write_split_config(&loaded, false)
        .await
        .expect("test: save");
    let saved = saved_text(dir.path());
    assert!(
        saved.contains("schema_version = 2") && saved.contains("enabled = true"),
        "{saved}"
    );
}

/// D-073: in a file saved with the versioned schema, an explicit
/// `extra_latency_budget_ms = 300` and `enabled = false` survive load,
/// save and reload; the same budget in a pre-1.7 file takes the new
/// default.
#[tokio::test]
async fn an_explicit_value_in_a_versioned_file_is_never_rewritten() {
    let (_legacy_dir, legacy) = load(SAVED_BY_8A5D160D);
    assert_eq!(
        legacy.expect("test: loads").causal_tree.policy.extra_latency_budget_ms,
        500
    );

    let (dir, loaded) = load(
        "default_temperature = 0.7\n\n[causal_tree]\nschema_version = 1\nenabled = false\n\n\
             [causal_tree.policy]\nextra_latency_budget_ms = 300\n",
    );
    let loaded = loaded.expect("test: loads");
    assert_eq!(loaded.causal_tree.policy.extra_latency_budget_ms, 300);
    assert!(!loaded.causal_tree.enabled);
    crate::config::files::write_split_config(&loaded, false)
        .await
        .expect("test: save");
    assert!(saved_text(dir.path()).contains("extra_latency_budget_ms = 300"));
    let reloaded =
        Config::load_from_path(&dir.path().join("config.toml"), dir.path().join("workspace")).expect("test: reload");
    assert_eq!(reloaded.causal_tree.policy.extra_latency_budget_ms, 300);
    assert_eq!(reloaded.causal_tree, loaded.causal_tree);
}
