//! Compare decoder capacity policies with a fixed message limit and changing sizes.
//! Use isolated build/output directories and interleaved runs on each revision.

use std::hint::black_box;
use std::time::Instant;

use bytes::{Bytes, BytesMut};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use flate2::{Compress, Compression, FlushCompress};
use futures_util::StreamExt;
use sockudo_ws::deflate::{DeflateConfig, DeflateDecoder};
use sockudo_ws::frame::{OpCode, encode_frame_with_rsv};
use sockudo_ws::{CompressedWebSocketStream, Config};
use tokio::io::AsyncWriteExt;

const MAX_MESSAGE_SIZE: usize = 1024 * 1024;
const CASES: &[(&str, &[usize], bool)] = &[
    ("small", &[32], false),
    ("medium", &[4096], false),
    ("large", &[65536], false),
    ("mixed_large", &[65536], true),
    ("alternating", &[32, 65536], false),
    (
        "occasional_large",
        &[32, 32, 32, 32, 32, 32, 32, 65536],
        false,
    ),
];

fn bench_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("deflate_capacity_decode");
    for &(name, sizes, mixed) in CASES {
        for reset in [false, true] {
            let context = if reset { "reset" } else { "takeover" };
            group.throughput(Throughput::Elements(sizes.len() as u64));
            group.bench_function(BenchmarkId::new(name, context), |b| {
                let (payloads, first, repeated) = fixture(sizes, mixed, reset);
                let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, reset);
                for inputs in [&first, &repeated] {
                    for (input, expected) in inputs.iter().zip(&payloads) {
                        assert_eq!(
                            decoder
                                .decompress(input, MAX_MESSAGE_SIZE)
                                .unwrap()
                                .as_ref(),
                            expected.as_ref()
                        );
                    }
                }
                b.iter(|| {
                    for input in &repeated {
                        black_box(
                            decoder
                                .decompress(black_box(input), MAX_MESSAGE_SIZE)
                                .unwrap(),
                        );
                    }
                });
            });
        }
    }
    group.finish();
}

fn bench_tcp(c: &mut Criterion) {
    const MESSAGES: usize = 256;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let mut group = c.benchmark_group("deflate_capacity_tcp");
    for &(name, sizes, mixed) in CASES {
        for reset in [false, true] {
            let context = if reset { "reset" } else { "takeover" };
            group.throughput(Throughput::Elements(MESSAGES as u64));
            group.bench_function(BenchmarkId::new(name, context), |b| {
                let (payloads, first, repeated) = fixture(sizes, mixed, reset);
                let mut warmup = Vec::new();
                let mut cycle = BytesMut::new();
                for inputs in [&first, &repeated] {
                    for input in inputs {
                        let mut frame = BytesMut::new();
                        encode_frame_with_rsv(&mut frame, OpCode::Binary, input, true, None, true);
                        if warmup.len() >= sizes.len() {
                            cycle.extend_from_slice(&frame);
                        }
                        warmup.push(frame);
                    }
                }
                assert_eq!(MESSAGES % sizes.len(), 0);
                let wire = cycle.repeat(MESSAGES / sizes.len());
                b.iter_custom(|iterations| {
                    runtime.block_on(async {
                        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                        let mut sender =
                            tokio::net::TcpStream::connect(listener.local_addr().unwrap())
                                .await
                                .unwrap();
                        let (receiver, _) = listener.accept().await.unwrap();
                        sender.set_nodelay(true).unwrap();
                        receiver.set_nodelay(true).unwrap();
                        let config = Config::builder()
                            .auto_ping(false)
                            .idle_timeout(0)
                            .max_message_size(MAX_MESSAGE_SIZE)
                            .build();
                        let deflate = DeflateConfig {
                            server_no_context_takeover: reset,
                            client_no_context_takeover: reset,
                            ..DeflateConfig::default()
                        };
                        let mut receiver =
                            CompressedWebSocketStream::client(receiver, config, deflate);
                        for (frame, expected) in warmup.iter().zip(payloads.iter().cycle()) {
                            // Poll both ends to avoid blocking on a large warm-up frame.
                            let (write, read) =
                                tokio::join!(sender.write_all(frame), receiver.next());
                            write.unwrap();
                            assert_eq!(read.unwrap().unwrap().as_bytes(), expected);
                        }
                        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
                        let wire = wire.clone();
                        let producer = tokio::spawn(async move {
                            start_rx.await.unwrap();
                            for _ in 0..iterations {
                                sender.write_all(&wire).await.unwrap();
                            }
                        });
                        let start = Instant::now();
                        start_tx.send(()).unwrap();
                        for _ in 0..iterations {
                            for _ in 0..MESSAGES {
                                black_box(receiver.next().await.unwrap().unwrap());
                            }
                        }
                        producer.await.unwrap();
                        start.elapsed()
                    })
                });
            });
        }
    }
    group.finish();
}

fn fixture(sizes: &[usize], mixed: bool, reset: bool) -> (Vec<Bytes>, Vec<Bytes>, Vec<Bytes>) {
    let payloads: Vec<Bytes> = sizes
        .iter()
        .map(|&size| {
            let mut payload = Vec::with_capacity(size);
            let mut random = 0x1234_5678_9abc_def0u64;
            for index in 0..size {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                payload.push(if !mixed {
                    b'A'
                } else if index < size * 3 / 4 {
                    random as u8
                } else {
                    payload[index - size / 4]
                });
            }
            Bytes::from(payload)
        })
        .collect();
    let mut compressor = Compress::new(Compression::new(6), false);
    let [first, repeated] = std::array::from_fn(|_| {
        payloads
            .iter()
            .map(|payload| {
                if reset {
                    compressor.reset();
                }
                let mut output = Vec::with_capacity(payload.len() * 2 + 128);
                let before = compressor.total_in();
                compressor
                    .compress_vec(payload, &mut output, FlushCompress::Sync)
                    .unwrap();
                assert_eq!(compressor.total_in() - before, payload.len() as u64);
                assert!(output.ends_with(&[0, 0, 255, 255]));
                output.truncate(output.len() - 4);
                Bytes::from(output)
            })
            .collect()
    });
    // Replay whole cycles so each block sees the same dictionary suffix.
    (payloads, first, repeated)
}

criterion_group!(benches, bench_decode, bench_tcp);
criterion_main!(benches);
