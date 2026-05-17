//! Whole-tracker /proc snapshot. Numbers depend on the host's
//! process table, so this measures *regression direction*, not
//! absolute speed.

use criterion::{criterion_group, criterion_main, Criterion};

#[cfg(target_os = "linux")]
fn bench_procs_snapshot(c: &mut Criterion) {
    use std::hint::black_box;
    c.bench_function("procs_snapshot", |b| {
        b.iter(|| black_box(neotop::bench_api::run_procs_snapshot()));
    });
}

#[cfg(not(target_os = "linux"))]
fn bench_procs_snapshot(_c: &mut Criterion) {}

criterion_group!(benches, bench_procs_snapshot);
criterion_main!(benches);
