use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use std::hint::black_box;

fn bench_orbit_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("orbit_build");
    for n in [100usize, 500, 2000] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter(|| black_box(neotop::bench_api::run_orbit_build(n, n / 2)));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_orbit_build);
criterion_main!(benches);
