use criterion::{Criterion, criterion_group, criterion_main};
use openprx::identity::{AieosIdentity, aieos_to_system_prompt};

fn bench_identity_prompt_render(c: &mut Criterion) {
    let identity = AieosIdentity::default();
    c.bench_function("identity_prompt_render_default", |b| {
        b.iter(|| aieos_to_system_prompt(&identity));
    });
}

criterion_group!(benches, bench_identity_prompt_render);
criterion_main!(benches);
