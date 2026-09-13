//! What each entry point actually publishes to the provider, in tools and bytes.
//!
//! The tool catalog is re-serialized into every single request of every single
//! turn, so its size is a fixed per-turn tax on both latency and cost. This test
//! renders the real registry through the real capability router for the two
//! surfaces whose policies differ — the operator's terminal chat and an IM
//! channel session — prints the byte totals, and pins the channel default: the
//! operations/development tools must not be advertised to a conversation
//! partner, and naming one in `[tool_tiering] always_include` must bring it
//! back.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::expect_used)]

use openprx::config::{Config, DEFAULT_CHANNEL_EXCLUDED_TOOLS, MemoryConfig, ToolTieringConfig};
use openprx::memory::Memory;
use openprx::security::SecurityPolicy;
use openprx::tools::{Tool, ToolCatalog, ToolSpec, all_tools, intent};
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::TempDir;

/// A greeting-sized message: it names no capability, so intent routing may
/// activate nothing and the surface falls back to its unconditional tools.
const NEUTRAL_MESSAGE: &str = "thanks, that answers it";

fn registry(tmp: &TempDir) -> Vec<Box<dyn Tool>> {
    let security = Arc::new(SecurityPolicy::default());
    let memory_config = MemoryConfig {
        backend: "markdown".into(),
        ..MemoryConfig::default()
    };
    let memory: Arc<dyn Memory> =
        Arc::from(openprx::memory::create_memory(&memory_config, tmp.path(), None).expect("fixture memory"));
    let config = Config {
        workspace_dir: tmp.path().join("workspace"),
        config_path: tmp.path().join("config.toml"),
        ..Config::default()
    };
    all_tools(
        Arc::new(config.clone()),
        &security,
        memory,
        None,
        None,
        &config.browser,
        &config.http_request,
        tmp.path(),
        &HashMap::new(),
        None,
        &config,
    )
}

fn specs_for(tools: &[Box<dyn Tool>], message: &str, tiering: &ToolTieringConfig) -> Vec<ToolSpec> {
    let selected = intent::select_tools_for_intent(tools, message, &tiering.always_include, &tiering.always_exclude);
    ToolCatalog::from_tools(selected).tool_specs()
}

fn wire_bytes(specs: &[ToolSpec]) -> usize {
    serde_json::to_string(specs).expect("tool specs serialize").len()
}

fn names(specs: &[ToolSpec]) -> Vec<&str> {
    specs.iter().map(|spec| spec.name.as_str()).collect()
}

#[test]
fn channel_sessions_do_not_advertise_the_operations_surface() {
    let tmp = TempDir::new().unwrap();
    let tools = registry(&tmp);

    let operator = ToolTieringConfig::default();
    let channel = operator.for_channel_surface();

    let operator_specs = specs_for(&tools, NEUTRAL_MESSAGE, &operator);
    let channel_specs = specs_for(&tools, NEUTRAL_MESSAGE, &channel);

    println!(
        "tool surface budget (registry {} tools):\n  terminal/gateway: {} tools, {} wire bytes\n  IM channel:       {} tools, {} wire bytes",
        tools.len(),
        operator_specs.len(),
        wire_bytes(&operator_specs),
        channel_specs.len(),
        wire_bytes(&channel_specs),
    );

    let channel_names = names(&channel_specs);
    for excluded in DEFAULT_CHANNEL_EXCLUDED_TOOLS {
        assert!(
            !channel_names.contains(excluded),
            "`{excluded}` must not be advertised to an IM channel session: {channel_names:?}"
        );
    }
    assert!(
        channel_specs.len() <= operator_specs.len(),
        "the channel surface may never be wider than the operator surface"
    );

    // A neutral message is the weak case: intent routing already drops most of
    // this list. The exclusion only earns its place when the message *does* name
    // those capabilities, so drive the full operator surface and diff the two.
    let broad = "check the git plugins, the proxy config, the mcp status, the skills and reindex the documents";
    let operator_broad = specs_for(&tools, broad, &operator);
    let channel_broad = specs_for(&tools, broad, &channel);
    let operator_broad_names = names(&operator_broad);
    let channel_broad_names = names(&channel_broad);
    let reachable: Vec<&str> = DEFAULT_CHANNEL_EXCLUDED_TOOLS
        .iter()
        .copied()
        .filter(|name| operator_broad_names.contains(name))
        .collect();
    assert!(
        reachable.len() >= 5,
        "fixture must exercise several excluded tools, reached {reachable:?} of {operator_broad_names:?}"
    );
    for excluded in &reachable {
        assert!(
            !channel_broad_names.contains(excluded),
            "`{excluded}` reached the operator surface and must still be hidden from a channel: {channel_broad_names:?}"
        );
    }
    println!(
        "capability-naming message:\n  terminal/gateway: {} tools, {} wire bytes\n  IM channel:       {} tools, {} wire bytes (hidden: {:?})",
        operator_broad.len(),
        wire_bytes(&operator_broad),
        channel_broad.len(),
        wire_bytes(&channel_broad),
        reachable,
    );
}

/// The default is a default, not a ceiling: an operator who wants a repository
/// or plugin tool on a channel names it and gets it back.
///
/// MUTATION GUARD: make `for_channel_surface` ignore `always_include` and this
/// test goes red.
#[test]
fn always_include_restores_a_channel_excluded_tool() {
    let tmp = TempDir::new().unwrap();
    let tools = registry(&tmp);

    let mut operator = ToolTieringConfig::default();
    operator.always_include.push("git_operations".to_string());
    let channel = operator.for_channel_surface();

    // `git_operations` is DevOps-tiered, so name the capability too: the point
    // of this test is the override, not the classifier.
    let channel_specs = specs_for(&tools, "commit this to git", &channel);
    let channel_names = names(&channel_specs);
    assert!(
        channel_names.contains(&"git_operations"),
        "always_include must survive the channel default: {channel_names:?}"
    );
}

/// Emptying the list turns the whole default off, which is the documented
/// escape hatch for an operator-only deployment.
#[test]
fn an_empty_channel_exclude_list_restores_the_full_surface() {
    let tmp = TempDir::new().unwrap();
    let tools = registry(&tmp);

    let operator = ToolTieringConfig {
        channel_exclude: Vec::new(),
        ..ToolTieringConfig::default()
    };
    let channel = operator.for_channel_surface();
    assert!(channel.always_exclude.is_empty(), "nothing may be folded in");

    let message = "check the plugins and the nodes";
    let channel_specs = specs_for(&tools, message, &channel);
    let operator_specs = specs_for(&tools, message, &operator);
    assert_eq!(
        names(&channel_specs),
        names(&operator_specs),
        "an empty channel_exclude must leave the surface untouched"
    );
}

/// Capability routing must still be what decides the rest of the catalog: an
/// operations request on a channel drops the excluded names and keeps the ones
/// the message actually asked for.
#[test]
fn channel_exclusion_survives_a_capability_matching_message() {
    let tmp = TempDir::new().unwrap();
    let tools = registry(&tmp);
    let channel = ToolTieringConfig::default().for_channel_surface();

    let message = "reload the wasm plugins and check the proxy config on the remote node";
    let operator_specs = specs_for(&tools, message, &ToolTieringConfig::default());
    let channel_specs = specs_for(&tools, message, &channel);
    println!(
        "operations-shaped message:\n  terminal/gateway: {} tools, {} wire bytes\n  IM channel:       {} tools, {} wire bytes",
        operator_specs.len(),
        wire_bytes(&operator_specs),
        channel_specs.len(),
        wire_bytes(&channel_specs),
    );

    let channel_names = names(&channel_specs);
    let operator_names = names(&operator_specs);
    for excluded in ["proxy_config", "skills_manage", "mcp_status"] {
        assert!(
            operator_names.contains(&excluded),
            "fixture is useless unless the operator surface offers `{excluded}`: {operator_names:?}"
        );
        assert!(
            !channel_names.contains(&excluded),
            "`{excluded}` must stay hidden even when the message names its capability: {channel_names:?}"
        );
    }
    assert!(
        channel_names.contains(&"xin"),
        "unrelated Automation tools must still route normally: {channel_names:?}"
    );
    assert!(
        wire_bytes(&channel_specs) < wire_bytes(&operator_specs),
        "the channel surface must be strictly cheaper on an operations-shaped message"
    );
}
