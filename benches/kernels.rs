//! See benches/README.md for measurement contracts and feature selection.

use criterion::{Criterion, criterion_group, criterion_main};

#[path = "support/client_mask_bench.rs"]
pub mod client_mask_bench;
#[path = "support/comparison_bench.rs"]
pub mod comparison_bench;
#[path = "support/masking_bench.rs"]
pub mod masking_bench;
#[path = "support/websocket_bench.rs"]
pub mod websocket_bench;

fn benchmarks(c: &mut Criterion) {
    #[cfg(feature = "tokio-runtime")]
    sockudo_ws::init_clock();

    c.bench_function("kernels/core/mask_rng", |b| {
        b.iter(|| std::hint::black_box(sockudo_ws::mask::generate_mask()))
    });

    websocket_bench::bench_mask_alignment(c);
    websocket_bench::bench_utf8(c);
    websocket_bench::bench_parse(c);
    websocket_bench::bench_encode(c);
    comparison_bench::bench_masking_comparison(c);
    masking_bench::bench_parse(c);
    client_mask_bench::bench_encoding(c);
}

criterion_group!(benches, benchmarks);
criterion_main!(benches);
