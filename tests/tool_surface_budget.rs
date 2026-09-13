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

/// A greeting-sized message. It names no capability, so routing is *unrouted*
/// and falls back to the whole registry — which is exactly why the channel
/// exclusion below has to be a policy boundary and not a routing side effect.
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

/// What a stateless entry point (IM channel, gateway, console, worker) puts on
/// the wire for one turn: an unrouted turn there has nothing to fall back on,
/// so it publishes everything operator policy allows.
fn specs_for(tools: &[Box<dyn Tool>], message: &str, tiering: &ToolTieringConfig) -> Vec<ToolSpec> {
    specs_with_policy(tools, message, tiering, intent::UnroutedToolPolicy::PublishEverything)
}

fn specs_with_policy(
    tools: &[Box<dyn Tool>],
    message: &str,
    tiering: &ToolTieringConfig,
    unrouted: intent::UnroutedToolPolicy,
) -> Vec<ToolSpec> {
    let selected = intent::resolve_tools_for_intent(
        tools,
        message,
        &tiering.always_include,
        &tiering.always_exclude,
        unrouted,
    );
    ToolCatalog::from_tools(selected).tool_specs()
}

/// One chat turn, through both halves of the production decision: the
/// dispatcher folds the turn into the session's cumulative exposure, then the
/// shared tool loop re-routes the same text under that surface. Skipping the
/// second half hides the defect this models — the loop's own unrouted fallback
/// used to re-widen a surface the session had deliberately held still.
fn chat_turn_specs(
    exposure: &intent::SessionToolExposure,
    tools: &[Box<dyn Tool>],
    message: &str,
    base: &ToolTieringConfig,
) -> Vec<ToolSpec> {
    let surface = exposure.sticky_surface(base, tools, message);
    specs_with_policy(tools, message, &surface.tiering, surface.unrouted)
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

    // The neutral case above is carried entirely by `always_exclude`. Prove the
    // exclusion also survives a message that explicitly names those
    // capabilities, which is the case routing would otherwise let through.
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

/// What a real request actually gets, against the real default registry.
///
/// Every existing routing test uses synthetic probe tools or asserts on
/// categories, so none of them could see a request that reaches the model
/// without the tool it obviously needs. These name the tools by hand.
#[test]
fn common_requests_reach_the_tools_they_obviously_need() {
    let tmp = TempDir::new().unwrap();
    let tools = registry(&tmp);
    let tiering = ToolTieringConfig::default();

    let cases: [(&str, &[&str]); 8] = [
        ("commit this change and push the branch", &["git_operations", "shell"]),
        ("search the web for the rustls release notes", &["web_search_tool"]),
        (
            "fetch https://example.com/report and summarise it",
            &["web_fetch", "http_request"],
        ),
        ("remember that I prefer tabs over spaces", &["memory_store"]),
        ("schedule a cron job that runs this every morning", &["cron"]),
        ("send a notification about it", &["pushover"]),
        (
            "read the file in that directory and fix the typo",
            &["file_read", "file_edit"],
        ),
        ("check the mcp server status", &["mcp_status"]),
    ];

    for (message, expected) in cases {
        let specs = specs_for(&tools, message, &tiering);
        let got = names(&specs);
        for tool in expected {
            assert!(got.contains(tool), "`{message}` must reach `{tool}`, got {got:?}");
        }
    }
}

/// The keyword table is English-only on purpose, so a request it cannot read is
/// not evidence for trimming anything: the surface falls back to the whole
/// registry rather than to the Core floor.
///
/// MUTATION GUARD: make `select_tools_for_intent` return a routed set for an
/// empty category set, or make `resolve_tools_for_intent` ignore
/// `PublishEverything`, and every one of these goes red.
#[test]
fn a_request_the_keyword_table_cannot_read_keeps_the_whole_registry() {
    let tmp = TempDir::new().unwrap();
    let tools = registry(&tmp);
    let tiering = ToolTieringConfig::default();
    // The unrouted surface is the whole published catalog; `tools.len()` is not
    // the same number because a registry entry may publish several names.
    let baseline = specs_for(&tools, "", &tiering);
    let everything = names(&baseline);
    assert!(
        everything.contains(&"cron") && everything.contains(&"git_operations"),
        "the fixture's baseline must really be the whole catalog: {everything:?}"
    );

    // Three ordinary requests that contain no English capability keyword: two
    // non-English, one English small talk.
    let unrouted = [
        "\u{5e2e}\u{6211}\u{628a}\u{8fd9}\u{6bb5}\u{6539}\u{5f97}\u{66f4}\u{7b80}\u{6d01}\u{4e00}\u{70b9}",
        "\u{3053}\u{306e}\u{6587}\u{7ae0}\u{3092}\u{77ed}\u{304f}\u{3057}\u{3066}\u{304f}\u{3060}\u{3055}\u{3044}",
        "could you make that sound a little friendlier please",
    ];
    for message in unrouted {
        let specs = specs_for(&tools, message, &tiering);
        let got = names(&specs);
        assert_eq!(
            got,
            everything,
            "an unrouted request must keep the whole registry, got {} of {} tools",
            got.len(),
            everything.len()
        );
    }

    // ... and operator policy still outranks the fallback.
    let restricted = ToolTieringConfig {
        always_exclude: vec!["cron".to_string()],
        ..ToolTieringConfig::default()
    };
    let restricted_specs = specs_for(&tools, unrouted[0], &restricted);
    let got = names(&restricted_specs);
    assert!(
        !got.contains(&"cron"),
        "always_exclude must survive the fallback: {got:?}"
    );
}

/// An English request that *does* name a capability is still narrowed — the
/// fallback is a floor for unreadable input, not a retreat from routing.
#[test]
fn a_routed_english_request_is_still_narrowed() {
    let tmp = TempDir::new().unwrap();
    let tools = registry(&tmp);
    let tiering = ToolTieringConfig::default();

    let routed_specs = specs_for(&tools, "commit this change", &tiering);
    let routed = names(&routed_specs);
    let baseline = specs_for(&tools, "", &tiering);
    let everything = names(&baseline);
    assert!(
        routed.len() < everything.len(),
        "naming a capability must still drop the rest of the catalog: {} vs {}",
        routed.len(),
        everything.len()
    );
    assert!(routed.contains(&"git_operations"), "{routed:?}");
    assert!(
        !routed.contains(&"cron"),
        "an unnamed Extended capability must stay out: {routed:?}"
    );
}

/// Three turns of one real chat session, against the real default registry.
///
/// The third turn is a closing pleasantry: it names no capability, so routing
/// reads nothing. A session that already knows what it exposed must publish
/// that same set again. Answering "the whole registry" there is what made a
/// captured session jump from 16 tools to the full catalog on turn 3 and stay
/// there for every later turn.
///
/// MUTATION GUARD: absorb the unrouted fallback unconditionally in
/// `sticky_surface`, or return `PublishEverything` from it, and the turn-3
/// assertions go red.
#[test]
fn a_closing_pleasantry_does_not_widen_an_established_chat_session() {
    let tmp = TempDir::new().unwrap();
    let tools = registry(&tmp);
    let base = ToolTieringConfig::default();
    let everything = names(&specs_for(&tools, "", &base))
        .iter()
        .map(|name| (*name).to_string())
        .collect::<Vec<_>>();

    let exposure = intent::SessionToolExposure::new();
    let turn1 = chat_turn_specs(
        &exposure,
        &tools,
        "Search the web for the latest Rust release notes.",
        &base,
    );
    let turn2 = chat_turn_specs(
        &exposure,
        &tools,
        "Now commit the changes in this repository with git.",
        &base,
    );
    let closing = "Thanks, that is all.";
    let turn3 = chat_turn_specs(&exposure, &tools, closing, &base);

    let turn1_names = names(&turn1);
    let turn2_names = names(&turn2);
    let turn3_names = names(&turn3);

    // Fixture self-check 1: the first two turns really are routed, and really
    // are far away from the whole catalog — otherwise "turn3 == turn2" would be
    // satisfied by a fixture that never narrows anything.
    assert!(
        turn1_names.contains(&"web_search_tool"),
        "turn 1 must reach web search: {turn1_names:?}"
    );
    assert!(
        turn2_names.contains(&"git_operations") && turn2_names.contains(&"web_search_tool"),
        "turn 2 must add git and keep web search: {turn2_names:?}"
    );
    assert!(
        turn2_names.len() > turn1_names.len(),
        "turn 2 must widen the session: {} vs {}",
        turn1_names.len(),
        turn2_names.len()
    );
    assert!(
        turn2_names.len() + 10 < everything.len(),
        "the fixture must stay well clear of the whole catalog, otherwise the turn-3 assertion is vacuous: {} vs {}",
        turn2_names.len(),
        everything.len()
    );

    // Fixture self-check 2: the closing line really is unrouted — a stateless
    // entry point still falls back to the whole registry for it.
    let stateless_specs = specs_for(&tools, closing, &base);
    assert_eq!(
        names(&stateless_specs),
        everything,
        "the closing line must be genuinely unrouted"
    );

    assert_eq!(
        turn3_names,
        turn2_names,
        "an unrouted turn must publish the session's set unchanged, got {} tools instead of {}",
        turn3_names.len(),
        turn2_names.len()
    );
    assert!(
        turn3_names.len() < everything.len(),
        "the session must not be pinned at the whole catalog by one pleasantry: {} of {}",
        turn3_names.len(),
        everything.len()
    );

    // Wire bytes are the whole point of the narrowing, so pin that too.
    assert!(
        wire_bytes(&turn3) < wire_bytes(&specs_for(&tools, "", &base)),
        "turn 3 must not pay for the whole catalog: {} bytes",
        wire_bytes(&turn3),
    );
}

/// The protection for an unreadable *first* turn survives. A session with
/// nothing absorbed has no earlier decision to keep, so it gets the whole
/// registry — a non-English opener may not lose its tools — and the union then
/// only grows.
///
/// MUTATION GUARD: drop the `names.is_empty()` branch in `sticky_surface` and
/// the first assertion goes red.
#[test]
fn a_chat_session_opening_on_an_unrouted_turn_keeps_the_whole_registry() {
    let tmp = TempDir::new().unwrap();
    let tools = registry(&tmp);
    let base = ToolTieringConfig::default();
    let everything = names(&specs_for(&tools, "", &base))
        .iter()
        .map(|name| (*name).to_string())
        .collect::<Vec<_>>();

    let exposure = intent::SessionToolExposure::new();
    let opener = chat_turn_specs(&exposure, &tools, "Please summarize what you can do.", &base);
    assert_eq!(
        names(&opener),
        everything,
        "an unreadable first turn may not lose capabilities"
    );

    for follow_up in [
        "Now commit the changes in this repository with git.",
        "\u{5e2e}\u{6211}\u{770b}\u{4e00}\u{4e0b}\u{8fd9}\u{4e2a}",
        "Thanks, that is all.",
    ] {
        let specs = chat_turn_specs(&exposure, &tools, follow_up, &base);
        assert_eq!(
            names(&specs),
            everything,
            "the union only grows, so every later turn stays at the full catalog"
        );
    }
}

/// Channels, gateway, console and worker turns keep R1's behaviour exactly:
/// each message is its own session, there is no earlier decision to keep, so an
/// unrouted turn publishes everything operator policy allows.
///
/// MUTATION GUARD: make `PublishEverything` behave like `KeepPinnedExposure`
/// and both assertions go red.
#[test]
fn a_stateless_entry_point_still_publishes_everything_on_an_unrouted_turn() {
    let tmp = TempDir::new().unwrap();
    let tools = registry(&tmp);
    let channel = ToolTieringConfig::default().for_channel_surface();

    let allowed_specs = specs_for(&tools, "", &channel);
    let allowed = names(&allowed_specs);
    assert!(
        !allowed.contains(&"proxy_config"),
        "the channel surface must still hide the operations tools: {allowed:?}"
    );

    for message in [
        "Thanks, that is all.",
        "\u{3053}\u{306e}\u{6587}\u{7ae0}\u{3092}\u{77ed}\u{304f}\u{3057}\u{3066}",
    ] {
        let specs = specs_for(&tools, message, &channel);
        assert_eq!(
            names(&specs),
            allowed,
            "a channel turn the keyword table cannot read must keep every tool it is allowed"
        );
    }
}
