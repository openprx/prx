//! CF-R8: subscription usage is counted. `prx chat --plain` on the
//! `kimi-code` subscription provider, pointed at a loopback OpenAI-compatible
//! listener (real HTTP/SSE bytes carrying `usage`), with a real config dir,
//! SQLite store and cost ledger.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/compaction_script.rs"]
mod compaction_script;
#[path = "support/loopback_openai.rs"]
mod loopback_openai;
#[path = "support/plain_chat.rs"]
mod plain_chat;

use std::time::{Duration, Instant};

use loopback_openai::{LoopbackOpenAi, Scripted, write_config};
use plain_chat::{Running, Sandbox};

const PROVIDER: &str = "kimi-code";
const MODEL: &str = "kimi-for-coding";
/// Wire usage of every completion: prompt, completion (total 19).
const USAGE: (u64, u64) = (12, 7);
/// Longest wait for one turn's persisted reply.
const TURN_DEADLINE: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_millis(50);

fn wait_for_replies(sandbox: &Sandbox, count: usize) {
    let started = Instant::now();
    while compaction_script::assistant_reply_count(&sandbox.workspace) < count {
        assert!(
            started.elapsed() < TURN_DEADLINE,
            "turn {count} did not complete within TURN_DEADLINE"
        );
        std::thread::sleep(POLL);
    }
}

fn ledger_settlements(sandbox: &Sandbox) -> Vec<String> {
    std::fs::read_to_string(sandbox.workspace.join("state/costs.jsonl"))
        .expect("test: cost ledger")
        .lines()
        .map(|line| {
            let row: serde_json::Value = serde_json::from_str(line).expect("test: ledger row");
            row["settlement_id"].as_str().expect("test: settlement id").to_owned()
        })
        .collect()
}

fn cost_command(sandbox: &Sandbox, args: &[&str]) -> String {
    let output = Running::spawn(sandbox.command(args)).wait();
    assert!(output.status.success(), "prx cost failed; stderr: {}", output.stderr);
    output.stdout
}

#[test]
fn plain_turns_record_one_subscription_row_each() {
    let sandbox = Sandbox::new();
    let server = LoopbackOpenAi::start(
        MODEL,
        Some(USAGE),
        Box::new(|_, _| Scripted::text("PRX-SUBSCRIPTION-OK")),
    );
    write_config(&sandbox.config_dir, PROVIDER, MODEL, |config| {
        config.api_url = Some(server.base_url.clone());
        // No `[cost].prices` override: the catalog bills kimi-code by subscription.
        config.cost.prices.clear();
        config.autonomy.level = openprx::security::AutonomyLevel::Full;
    });

    let mut chat = Running::spawn(sandbox.command(&["chat", "--plain"]));
    chat.send("First subscription turn.");
    wait_for_replies(&sandbox, 1);
    chat.send("Second subscription turn.");
    wait_for_replies(&sandbox, 2);
    chat.send("/quit");
    let output = chat.wait();
    assert!(output.status.success(), "chat failed; stderr: {}", output.stderr);
    assert!(server.requests().len() >= 2, "both turns reached the loopback provider");

    let report: serde_json::Value =
        serde_json::from_str(&cost_command(&sandbox, &["cost", "--json"])).expect("test: cost JSON");
    let all_time = &report["all_time"];
    assert_eq!(all_time["requests"], 2, "{report}");
    assert_eq!(all_time["total_tokens"], 2 * (USAGE.0 + USAGE.1), "{report}");
    assert_eq!(all_time["subscription_requests"], 2, "{report}");
    assert_eq!(all_time["cost_usd"].as_f64(), Some(0.0), "{report}");
    let by_model = report["by_model"].as_object().expect("test: by_model");
    assert!(
        by_model.values().any(|model| model["billing"] == "subscription"),
        "{report}"
    );

    let text = cost_command(&sandbox, &["cost"]);
    assert!(text.contains("(2 subscription-billed; no per-token cost)"), "{text}");

    // A second start runs the startup projector pass: still one row per terminal.
    let mut restarted = Running::spawn(sandbox.command(&["chat", "--plain"]));
    restarted.send("/quit");
    let output = restarted.wait();
    assert!(
        output.status.success(),
        "restarted chat failed; stderr: {}",
        output.stderr
    );
    let settlements = ledger_settlements(&sandbox);
    assert_eq!(settlements.len(), 2, "{settlements:?}");
    assert_ne!(settlements.first(), settlements.get(1), "one row per terminal id");
}
