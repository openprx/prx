use criterion::{Criterion, criterion_group, criterion_main};
use openprx::llm::route_decision::RouteDecision;
use openprx::router::automix::ConfidenceChecker;

fn bench_route_decision_single_candidate(c: &mut Criterion) {
    c.bench_function("route_decision_single_candidate_for_context", |b| {
        b.iter(|| {
            RouteDecision::single_candidate_for_context(
                "kimi-code",
                "kimi2.6",
                "owner:bench",
                "session:bench",
                Some("msg-bench".to_string()),
                Some("kimi-code/kimi2.6".to_string()),
                "code",
                4_096,
                true,
                true,
            )
        });
    });
}

fn bench_confidence_rules(c: &mut Criterion) {
    c.bench_function("router_confidence_rules_code_answer", |b| {
        b.iter(|| {
            ConfidenceChecker::check_rules(
                "```rust\nfn main() { println!(\"ok\"); }\n```\nThis compiles.",
                "Please fix this Rust code and explain why it works.",
            )
        });
    });
}

criterion_group!(benches, bench_route_decision_single_candidate, bench_confidence_rules);
criterion_main!(benches);
