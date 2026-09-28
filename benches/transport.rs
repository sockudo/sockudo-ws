//! See benches/README.md for measurement contracts and feature selection.

use criterion::{Criterion, criterion_group, criterion_main};

#[cfg(feature = "tokio-runtime")]
#[path = "support/client_mask_bench.rs"]
pub mod client_mask_bench;
#[cfg(all(feature = "tokio-runtime", feature = "permessage-deflate"))]
#[path = "support/deflate_capacity_bench.rs"]
pub mod deflate_capacity_bench;
#[cfg(all(feature = "tokio-runtime", feature = "permessage-deflate"))]
#[path = "support/deflate_input_bench.rs"]
pub mod deflate_input_bench;
#[cfg(feature = "tokio-runtime")]
#[path = "support/masking_bench.rs"]
pub mod masking_bench;
#[cfg(feature = "tokio-runtime")]
#[path = "support/sink_backpressure.rs"]
pub mod sink_backpressure;
#[path = "support/split_receive.rs"]
pub mod split_receive;
#[cfg(feature = "tokio-runtime")]
#[path = "support/stream_bench.rs"]
pub mod stream_bench;

fn benchmarks(_c: &mut Criterion) {
    #[cfg(feature = "tokio-runtime")]
    sockudo_ws::init_clock();

    #[cfg(all(feature = "tokio-runtime", feature = "http2"))]
    adapters::h2(_c);
    #[cfg(all(feature = "tokio-runtime", feature = "http3"))]
    adapters::h3(_c);

    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    uring_cases::benchmark(_c);
    #[cfg(any(
        feature = "rustls-webpki-roots",
        feature = "rustls-native-roots",
        feature = "rustls-platform-verifier"
    ))]
    tls_cases::benchmark(_c);
    #[cfg(feature = "tokio-runtime")]
    socket_cases::tokio_tcp(_c);
    #[cfg(all(feature = "tokio-runtime", feature = "http2"))]
    socket_cases::tokio_http2(_c);
    #[cfg(all(feature = "tokio-runtime", feature = "http3"))]
    socket_cases::tokio_http3(_c);
    #[cfg(feature = "compio-runtime")]
    socket_cases::compio_tcp(_c);
    #[cfg(all(feature = "compio-runtime", feature = "http2"))]
    socket_cases::compio_http2(_c);
    #[cfg(all(feature = "compio-runtime", feature = "http3"))]
    socket_cases::compio_http3(_c);

    #[cfg(feature = "tokio-runtime")]
    stream_bench::bench_stream(_c);
    #[cfg(feature = "tokio-runtime")]
    stream_bench::bench_masked_receive(_c);
    #[cfg(feature = "tokio-runtime")]
    sink_backpressure::bench_backpressure(_c);
    #[cfg(feature = "tokio-runtime")]
    masking_bench::bench_tcp(_c);
    #[cfg(feature = "tokio-runtime")]
    client_mask_bench::bench_tcp(_c);
    #[cfg(all(feature = "tokio-runtime", feature = "permessage-deflate"))]
    deflate_input_bench::bench_tcp(_c);
    #[cfg(all(feature = "tokio-runtime", feature = "permessage-deflate"))]
    deflate_capacity_bench::bench_tcp(_c);
    #[cfg(any(feature = "tokio-runtime", feature = "compio-runtime"))]
    split_receive::tcp_cases(_c);
}

criterion_group!(benches, benchmarks);
criterion_main!(benches);

#[cfg(any(feature = "tokio-runtime", feature = "compio-runtime"))]
#[path = "support/socket_cases.rs"]
pub mod socket_cases;
#[cfg(any(
    feature = "http3",
    feature = "rustls-webpki-roots",
    feature = "rustls-native-roots",
    feature = "rustls-platform-verifier"
))]
#[path = "support/tls_fixture.rs"]
pub mod tls_fixture;

#[cfg(any(
    feature = "rustls-webpki-roots",
    feature = "rustls-native-roots",
    feature = "rustls-platform-verifier"
))]
#[path = "support/tls_cases.rs"]
pub mod tls_cases;

#[cfg(all(feature = "io-uring", target_os = "linux"))]
#[path = "support/uring_cases.rs"]
pub mod uring_cases;

#[cfg(all(feature = "tokio-runtime", any(feature = "http2", feature = "http3")))]
#[path = "support/adapters.rs"]
pub mod adapters;
