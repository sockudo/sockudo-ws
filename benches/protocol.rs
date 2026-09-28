//! See benches/README.md for measurement contracts and feature selection.

use criterion::{Criterion, criterion_group, criterion_main};

#[path = "support/comparison_bench.rs"]
pub mod comparison_bench;
#[path = "support/fragment_validation.rs"]
pub mod fragment_validation;
#[path = "support/protocol_bench.rs"]
pub mod protocol_bench;
#[cfg(feature = "permessage-deflate")]
#[path = "support/receive_limits.rs"]
pub mod receive_limits;
#[path = "support/split_receive.rs"]
pub mod split_receive;

fn benchmarks(c: &mut Criterion) {
    #[cfg(feature = "tokio-runtime")]
    sockudo_ws::init_clock();

    protocol_bench::bench_receive_container(c);
    protocol_bench::bench_fragmented_text(c);
    protocol_bench::bench_write_slices(c);
    fragment_validation::bench_fragments(c);
    comparison_bench::bench_message_protocol(c);
    #[cfg(feature = "permessage-deflate")]
    receive_limits::bench_receive_limits(c);
    split_receive::parser_cases(c);
}

criterion_group!(benches, benchmarks);
criterion_main!(benches);
