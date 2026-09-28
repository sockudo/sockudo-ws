//! See benches/README.md for measurement contracts and feature selection.

use criterion::{Criterion, criterion_group, criterion_main};

#[path = "support/websocket_bench.rs"]
pub mod websocket_bench;

fn benchmarks(c: &mut Criterion) {
    #[cfg(feature = "tokio-runtime")]
    sockudo_ws::init_clock();

    lifecycle::syntax(c);
    #[cfg(feature = "compio-runtime")]
    lifecycle::compio(c);
    #[cfg(feature = "tokio-runtime")]
    lifecycle::tokio(c);
    websocket_bench::bench_handshake(c);
}

criterion_group!(benches, benchmarks);
criterion_main!(benches);
#[path = "support/lifecycle.rs"]
pub mod lifecycle;

#[cfg(any(feature = "compio-runtime", feature = "tokio-runtime"))]
#[path = "support/controlled_io.rs"]
pub mod controlled_io;
