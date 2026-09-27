//! Deflate input allocation/copy experiments with dictionary and TCP coverage.
//!
//! Use the same toolchain/features and independent CARGO_TARGET_DIR and
//! CRITERION_HOME directories for each revision. Shared-host measurements are
//! exploratory; repeat interleaved comparisons before drawing conclusions.

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

fn fixture(size: usize, mixed: bool, no_context_takeover: bool) -> (Vec<u8>, Bytes, Bytes) {
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
    let mut compressor = Compress::new(Compression::new(6), false);
    let [first, repeated] = std::array::from_fn(|_| {
        if no_context_takeover {
            compressor.reset();
        }
        let mut output = Vec::with_capacity(size * 2 + 128);
        let before = compressor.total_in();
        compressor
            .compress_vec(&payload, &mut output, FlushCompress::Sync)
            .unwrap();
        assert_eq!(compressor.total_in() - before, size as u64);
        assert!(output.ends_with(&[0, 0, 255, 255]));
        output.truncate(output.len() - 4);
        Bytes::from(output)
    });
    // The payload repeats exactly. After a sync-flush boundary, replaying the
    // second block preserves every dictionary reference relative to that period.
    (payload, first, repeated)
}

fn bench_decompression(c: &mut Criterion) {
    let mut group = c.benchmark_group("deflate_input_decode");
    for size in [32, 1024, 4096, 65536] {
        for mixed in [false, true] {
            for no_context_takeover in [false, true] {
                let pattern = if mixed { "mixed" } else { "repeat" };
                let context = if no_context_takeover {
                    "reset"
                } else {
                    "takeover"
                };
                group.throughput(Throughput::Bytes(size as u64));
                group.bench_function(
                    BenchmarkId::new(format!("{pattern}_{context}"), size),
                    |b| {
                        let (payload, first, repeated) = fixture(size, mixed, no_context_takeover);
                        let mut decoder = DeflateDecoder::new(
                            sockudo_ws::deflate::MAX_WINDOW_BITS,
                            no_context_takeover,
                        );
                        for input in [&first, &repeated] {
                            assert_eq!(decoder.decompress(input, size).unwrap().as_ref(), payload);
                        }
                        b.iter(|| {
                            black_box(decoder.decompress(black_box(&repeated), size).unwrap());
                        });
                    },
                );
            }
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
    let mut group = c.benchmark_group("deflate_input_tcp");
    for size in [32, 4096, 65536] {
        for mixed in [false, true] {
            for no_context_takeover in [false, true] {
                let pattern = if mixed { "mixed" } else { "repeat" };
                let context = if no_context_takeover {
                    "reset"
                } else {
                    "takeover"
                };
                group.throughput(Throughput::Elements(MESSAGES as u64));
                group.bench_function(
                    BenchmarkId::new(format!("{pattern}_{context}"), size),
                    |b| {
                        let (payload, first, repeated) = fixture(size, mixed, no_context_takeover);
                        let mut first_frame = BytesMut::new();
                        encode_frame_with_rsv(
                            &mut first_frame,
                            OpCode::Binary,
                            &first,
                            true,
                            None,
                            true,
                        );
                        let mut repeated_frame = BytesMut::new();
                        encode_frame_with_rsv(
                            &mut repeated_frame,
                            OpCode::Binary,
                            &repeated,
                            true,
                            None,
                            true,
                        );
                        let wire = repeated_frame.repeat(MESSAGES);
                        b.iter_custom(|iterations| {
                            runtime.block_on(async {
                                let listener =
                                    tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                                let mut sender =
                                    tokio::net::TcpStream::connect(listener.local_addr().unwrap())
                                        .await
                                        .unwrap();
                                let (receiver, _) = listener.accept().await.unwrap();
                                sender.set_nodelay(true).unwrap();
                                receiver.set_nodelay(true).unwrap();
                                let config =
                                    Config::builder().auto_ping(false).idle_timeout(0).build();
                                let deflate = DeflateConfig {
                                    server_no_context_takeover: no_context_takeover,
                                    client_no_context_takeover: no_context_takeover,
                                    ..DeflateConfig::default()
                                };
                                let mut receiver =
                                    CompressedWebSocketStream::client(receiver, config, deflate);
                                // Prime and verify the same history as the isolated decoder.
                                // Poll both sides so large warm-up frames cannot block setup.
                                for frame in [&first_frame, &repeated_frame] {
                                    let (write, read) =
                                        tokio::join!(sender.write_all(frame), receiver.next());
                                    write.unwrap();
                                    assert_eq!(read.unwrap().unwrap().as_bytes(), payload);
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
                    },
                );
            }
        }
    }
    group.finish();
}

criterion_group!(benches, bench_decompression, bench_tcp);
criterion_main!(benches);
