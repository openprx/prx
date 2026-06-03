//! Causal Tree Engine (CTE) — speculative multi-branch prediction for PRX.
//!
//! **Status: experimental, opt-in. Default off.** Enable via
//! `[causal_tree] enabled = true` (requires the `llm-router` feature).
//!
//! # Integration
//!
//! CTE is wired into the live agent tool loop (`agent::loop_::run`) through the
//! dedicated `BootstrapProfile::AgentLoop` (`runtime::bootstrap`). When enabled,
//! the engine is attached to the run's `AppContext` and, before each turn's tool
//! loop, `run_cte_prediction` builds a [`state::CausalState`] from the current
//! `ChatMessage` history and runs the pipeline
//! (`snapshot → expand → rehearse → score → select → feedback`). The whole call
//! is bounded by an outer `tokio::time::timeout(extra_latency_budget_ms)`; on
//! failure or timeout the turn falls back to the normal path unchanged.
//!
//! Only the `AgentLoop` profile attaches CTE. `prx chat` (`Interactive`) and all
//! other profiles never build it, so when disabled (the default) there is zero
//! cost on every path.
//!
//! # Cost
//!
//! Rehearsal runs in [`rehearsal::DefaultRehearsalEngine`], which performs **no
//! real I/O and makes no extra LLM calls** — it is an in-memory simulation. The
//! per-turn overhead is therefore bounded by the latency budget, not by extra
//! model round-trips.
//!
//! # v1 branch semantics (honest limitations)
//!
//! The chosen [`branch::BranchLabel`] influences the turn as follows in v1:
//!
//! - `DirectAnswer` — the default path; tracing only, execution flow unchanged.
//! - `RetrieveThenAnswer` — tracing only, execution flow unchanged. **Almost
//!   never reached in v1**: the expander's retrieval rule keys off retrieval
//!   keywords in `user_intent`, but `user_intent` is the classifier's three-state
//!   label (`simple`/`delegate`/`stream`), which never contains those keywords.
//!   Aligning intent → retrieval keywords is deferred to v2-next (it would change
//!   the expander's existing contract and tests).
//! - `AskApproval` — early exit: writes the assistant approval message to the
//!   `MemoryFabric`, emits the observer `TurnComplete` event, runs the
//!   `TurnComplete` hook, then returns (single-shot) / continues (interactive),
//!   skipping the tool loop. The **code path exists and is unit-tested** (via an
//!   injected risk that forces the branch), but it is **not reachable under the
//!   default config**: the snapshot's `unresolved_risks` is always empty and
//!   `default_side_effect_mode` is `ReadOnly`, so the expander has no risk input
//!   to select it. A real risk source (e.g. pending-approval write operations
//!   feeding `unresolved_risks`) is deferred to v2-next.
//!
//! In v1 the value of CTE is therefore: the pipeline genuinely runs, is
//! observable via observer `CteRun` events, and the `AskApproval` hook is wired
//! and tested — *not* a branch-driven rewrite of the execution flow.
//!
//! # Observability
//!
//! Runtime evidence that CTE actually ran is the observer `CteRun` event emitted
//! by [`engine::CausalTreeEngine::run`]. The control-ladder `causal_tree` layer is
//! a **config declaration only** (`attachment = config_snapshot`); it does not and
//! cannot prove runtime attachment — see `runtime::control_ladder`.

// Re-exports are used by lib consumers and by the loop_ integration.
#![allow(unused_imports)]

pub mod branch;
pub mod engine;
pub mod error;
pub mod expander;
pub mod feedback;
pub mod metrics;
pub mod policy;
pub mod rehearsal;
pub mod scorer;
pub mod selector;
pub mod snapshot;
pub mod state;

pub use branch::{BranchLabel, CausalBranch, CommitPolicy, CostEstimate, RehearsalLevel};
pub use engine::CausalTreeEngine;
pub use error::CausalTreeError;
pub use feedback::FeedbackWriter;
pub use metrics::CausalTreeMetrics;
pub use policy::{CausalPolicy, CausalTreeConfig};
pub use rehearsal::RehearsalEngine;
pub use scorer::BranchScorer;
pub use selector::PathSelector;
pub use state::{ArtifactRef, BudgetState, CausalState, RiskFlag, SideEffectMode, StepRecord};
