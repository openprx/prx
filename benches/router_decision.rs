use criterion::{Criterion, criterion_group, criterion_main};
use openprx::llm::route_decision::RouteDecision;

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

criterion_group!(benches, bench_route_decision_single_candidate);
criterion_main!(benches);
