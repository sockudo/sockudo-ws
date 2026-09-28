//! See benches/README.md for measurement contracts and feature selection.

use criterion::{Criterion, criterion_group, criterion_main};

#[path = "support/services_bench.rs"]
pub mod services_bench;

fn benchmarks(c: &mut Criterion) {
    #[cfg(feature = "tokio-runtime")]
    sockudo_ws::init_clock();

    pubsub_cases::benchmark(c);
    pubsub_cases::concurrent(c);
    services_bench::bench_publish(c);
    services_bench::bench_publish_with_churn(c);
}

criterion_group!(benches, benchmarks);
criterion_main!(benches);
#[path = "support/pubsub_cases.rs"]
pub mod pubsub_cases;
