//! Regression guard: the onboarding wizard must never construct a
//! `reqwest::blocking::Client` directly on a Tokio worker.
//!
//! `reqwest::blocking` enters and drops a temporary Tokio runtime on the calling
//! thread, which panics with "Cannot drop a runtime in a context where blocking
//! is not allowed" when that thread belongs to the async runtime. Every blocking
//! client in `src/onboard/wizard.rs` must therefore be created inside a
//! `std::thread::spawn` closure or a `runtime::blocking::spawn_blocking` closure.

#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::unwrap_used)]

const WIZARD: &str = include_str!("../src/onboard/wizard.rs");

/// How many lines above a client construction the off-runtime boundary may sit.
const BOUNDARY_LOOKBACK_LINES: usize = 6;

/// Top-level synchronous helpers that may build a blocking client without a
/// lexically adjacent boundary, because every async caller reaches them through
/// one. A new entry here is a review event, not a formality.
///
/// - `build_model_fetch_client`: private to the wizard and reachable only from
///   the `fetch_*` helpers behind `fetch_live_models_for_provider`, whose async
///   callers all go through `runtime::blocking::spawn_blocking`
///   (`fetch_live_models_for_provider_off_runtime` here, `src/runtime/mode.rs`
///   for `prx models refresh` and `prx doctor models`).
const ALLOWED_SYNC_BLOCKING_CLIENT_BUILDERS: &[&str] = &["build_model_fetch_client"];

/// Name of the nearest enclosing top-level function, or an empty string.
fn enclosing_top_level_function(lines: &[&str], index: usize) -> String {
    for candidate in (0..=index).rev() {
        let line = lines[candidate];
        for prefix in ["fn ", "pub fn ", "async fn ", "pub async fn "] {
            if let Some(rest) = line.strip_prefix(prefix) {
                return rest.split('(').next().unwrap_or_default().trim().to_string();
            }
        }
    }

    String::new()
}

#[test]
fn onboard_wizard_never_builds_a_blocking_client_on_the_async_worker() {
    let lines: Vec<&str> = WIZARD.lines().collect();
    let mut unguarded: Vec<String> = Vec::new();

    for (index, line) in lines.iter().enumerate() {
        if !line.contains("reqwest::blocking::Client::") {
            continue;
        }

        let start = index.saturating_sub(BOUNDARY_LOOKBACK_LINES);
        let guarded = lines[start..index]
            .iter()
            .any(|candidate| candidate.contains("spawn_blocking(") || candidate.contains("thread::spawn("));

        let owner = enclosing_top_level_function(&lines, index);
        let allowlisted = ALLOWED_SYNC_BLOCKING_CLIENT_BUILDERS.contains(&owner.as_str());

        if !guarded && !allowlisted {
            unguarded.push(format!("wizard.rs:{}: {}", index + 1, line.trim()));
        }
    }

    assert!(
        unguarded.is_empty(),
        "every reqwest::blocking client in the onboarding wizard must be built inside a \
         std::thread::spawn or runtime::blocking::spawn_blocking closure, so the client and its \
         response are dropped off the Tokio worker; unguarded sites:\n{}",
        unguarded.join("\n")
    );
}

#[test]
fn onboard_model_discovery_has_an_async_boundary() {
    assert!(
        WIZARD.contains("async fn fetch_live_models_for_provider_off_runtime("),
        "the wizard must expose an async boundary for the blocking model-discovery fan-out"
    );
    assert!(
        WIZARD.contains("crate::runtime::blocking::spawn_blocking("),
        "the model-discovery async boundary must route through runtime::blocking::spawn_blocking, \
         the same helper prx models refresh uses"
    );
    assert!(
        WIZARD.contains("async fn setup_provider("),
        "setup_provider must be async so it can await the off-runtime discovery boundary"
    );
}
