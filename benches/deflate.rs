//! See benches/README.md for measurement contracts and feature selection.

use criterion::{Criterion, criterion_group, criterion_main};

#[path = "support/deflate_capacity_bench.rs"]
pub mod deflate_capacity_bench;
#[path = "support/deflate_input_bench.rs"]
pub mod deflate_input_bench;
#[path = "support/services_bench.rs"]
pub mod services_bench;

fn benchmarks(c: &mut Criterion) {
    #[cfg(feature = "tokio-runtime")]
    sockudo_ws::init_clock();

    deflate_context::benchmark(c);
    deflate_input_bench::bench_decompression(c);
    deflate_capacity_bench::bench_decode(c);
    services_bench::bench_deflate(c);
    services_bench::bench_shared_compression(c);
}

criterion_group!(benches, benchmarks);
criterion_main!(benches);
#[path = "support/deflate_context.rs"]
pub mod deflate_context;

#[path = "support/corpus.rs"]
pub mod corpus;
