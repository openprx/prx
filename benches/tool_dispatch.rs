use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use openprx::security::SecurityPolicy;
use openprx::tools::default_tools;

fn bench_default_tool_registry_build(c: &mut Criterion) {
    let security = Arc::new(SecurityPolicy::default());
    c.bench_function("default_tool_registry_build", |b| {
        b.iter(|| default_tools(security.clone()));
    });
}

criterion_group!(benches, bench_default_tool_registry_build);
criterion_main!(benches);
