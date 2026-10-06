#![cfg(test)]

use std::hint::black_box;
use std::mem::{align_of_val, size_of_val};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::agent::dispatcher::XmlToolDispatcher;
use crate::agent::embedded_source::EmbeddedAgentBinding;
use crate::config::ConfigGeneration;
use crate::decision::builtin::allocation_counter::count;
use crate::memory::{Memory, SqliteMemory};
use crate::observability::NoopObserver;
use crate::providers::{ChatRequest, ChatResponse, Provider};

struct NeverCalledProvider(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl Provider for NeverCalledProvider {
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("constructor regression unexpectedly called provider")
    }

    async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
        self.0.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("constructor regression unexpectedly called provider")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FutureLayout {
    payload_bytes: usize,
    alignment: usize,
}

// These synchronous helpers return only scalars. Returning the raw future
// through count would introduce extra huge copies into the measurement stack.
#[inline(never)]
fn measure_raw(
    agent: &mut Agent,
    input: &str,
    generation: &Arc<ConfigGeneration>,
    binding: &Arc<EmbeddedAgentBinding>,
    cancellation: &CancellationToken,
) -> (FutureLayout, u64) {
    count(|| {
        let future = run_work(agent, input, generation, binding, cancellation);
        black_box(&future);
        let layout = FutureLayout {
            payload_bytes: size_of_val(&future),
            alignment: align_of_val(&future),
        };
        drop(future);
        layout
    })
}

#[inline(never)]
fn measure_boxed(
    agent: &mut Agent,
    input: &str,
    generation: &Arc<ConfigGeneration>,
    binding: &Arc<EmbeddedAgentBinding>,
    cancellation: &CancellationToken,
) -> ((FutureLayout, usize), u64) {
    count(|| {
        let future = boxed_turn_work(agent, input, generation, binding, cancellation);
        black_box(&future);
        let layout = FutureLayout {
            payload_bytes: size_of_val(future.as_ref().get_ref()),
            alignment: align_of_val(future.as_ref().get_ref()),
        };
        let handle_bytes = size_of_val(&future);
        drop(future);
        (layout, handle_bytes)
    })
}

#[inline(never)]
fn measure_outer(agent: &mut Agent, input: &str) -> (FutureLayout, u64) {
    count(|| {
        let future = agent.turn(input);
        black_box(&future);
        let layout = FutureLayout {
            payload_bytes: size_of_val(&future),
            alignment: align_of_val(&future),
        };
        drop(future);
        layout
    })
}

fn ownership_counts(
    agent: &Agent,
    generation: &Arc<ConfigGeneration>,
    binding: &Arc<EmbeddedAgentBinding>,
) -> [usize; 4] {
    [
        Arc::strong_count(generation),
        Arc::strong_count(binding),
        Arc::strong_count(&agent.provider),
        Arc::strong_count(&agent.memory),
    ]
}

fn assert_lazy_drop(
    agent: &Agent,
    generation: &Arc<ConfigGeneration>,
    binding: &Arc<EmbeddedAgentBinding>,
    cancellation: &CancellationToken,
    calls: &AtomicUsize,
    owners: [usize; 4],
) {
    assert!(matches!(agent.cleanup_state, CleanupState::Ready));
    assert!(agent.history.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!cancellation.is_cancelled());
    assert!(crate::runtime::turn_cleanup::current_tracker().is_none());
    assert_eq!(ownership_counts(agent, generation, binding), owners);
}

#[test]
#[allow(clippy::print_stderr)] // Emit only scalar evidence for the isolated allocation diagnostic.
fn embedded_work_box_has_one_constructor_allocation_and_unpolled_drop_is_lazy() {
    let workspace = tempfile::tempdir().expect("constructor fixture workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config.config_path = workspace.path().join("config.toml");
    config.default_model = Some("prx-local-model".to_owned());
    config.causal_tree.enabled = false;
    config.memory.backend = "sqlite".to_owned();
    let calls = Arc::new(AtomicUsize::new(0));
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.path()).expect("actual SQLite"));
    let mut agent = Agent::builder()
        .runtime_config(config, "ollama")
        .provider(Box::new(NeverCalledProvider(Arc::clone(&calls))))
        .tools(Vec::new())
        .memory(memory)
        .observer(Arc::new(NoopObserver {}))
        .tool_dispatcher(Box::new(XmlToolDispatcher))
        .build()
        .expect("actual embedded Agent binding");
    let generation = Arc::clone(agent.runtime_generation.as_ref().expect("actual pinned generation"));
    let binding = Arc::clone(agent.embedded_binding.as_ref().expect("actual source binding"));
    binding
        .verify(&generation, &agent.provider, &agent.model_name)
        .expect("matching actual instance");
    let provider_lifetime = Arc::downgrade(&agent.provider);
    let memory_lifetime = Arc::downgrade(&agent.memory);
    let cancellation = CancellationToken::new();
    let owners = ownership_counts(&agent, &generation, &binding);
    assert_lazy_drop(&agent, &generation, &binding, &cancellation, &calls, owners);

    let (raw, raw_allocations) = measure_raw(&mut agent, "unpolled source", &generation, &binding, &cancellation);
    assert_eq!(raw_allocations, 0);
    assert!(raw.payload_bytes > 0);
    assert_lazy_drop(&agent, &generation, &binding, &cancellation, &calls, owners);
    let ((boxed, handle_bytes), boxed_allocations) =
        measure_boxed(&mut agent, "unpolled source", &generation, &binding, &cancellation);
    assert_eq!(boxed_allocations, 1);
    assert_eq!(boxed, raw, "the box holds the same concrete work future layout");
    assert_eq!(handle_bytes, std::mem::size_of::<usize>());
    assert_lazy_drop(&agent, &generation, &binding, &cancellation, &calls, owners);
    let (outer, outer_allocations) = measure_outer(&mut agent, "unpolled source");
    assert_eq!(outer_allocations, 0, "outer admission must remain lazy");
    assert!(
        outer.payload_bytes < raw.payload_bytes,
        "outer storage should no longer inline the complete work future"
    );
    assert_lazy_drop(&agent, &generation, &binding, &cancellation, &calls, owners);
    assert!(!workspace.path().join("self/causal_tree/.decision_key").exists());
    assert!(!workspace.path().join("self/causal_tree/decisions.db").exists());

    // Explicit mutable access demonstrates that all three dropped futures have
    // released the Agent borrow, without polling any execution or cleanup work.
    agent.clear_history();
    assert!(agent.history.is_empty());
    eprintln!(
        "PRX_EMBEDDED_CONSTRUCTOR_LAYOUT raw_allocations={raw_allocations} boxed_allocations={boxed_allocations} outer_allocations={outer_allocations} raw_payload_bytes={} raw_alignment={} boxed_payload_bytes={} boxed_alignment={} boxed_handle_bytes={handle_bytes} outer_payload_bytes={} outer_alignment={}",
        raw.payload_bytes, raw.alignment, boxed.payload_bytes, boxed.alignment, outer.payload_bytes, outer.alignment
    );
    // These are constructor allocation counts and Rust payload/layout bytes,
    // not full-turn/process counts, allocator overhead, RSS or deallocations.
    drop(binding);
    drop(generation);
    drop(agent);
    assert!(provider_lifetime.upgrade().is_none());
    assert!(memory_lifetime.upgrade().is_none());
    assert_eq!(Arc::strong_count(&calls), 1);
}
