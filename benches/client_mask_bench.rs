//! Client copy-and-mask experiments, including complete frames and TCP delivery.
//!
//! Run `cargo bench --bench client_mask_bench` on each revision with the same
//! toolchain/features and separate CARGO_TARGET_DIR/CRITERION_HOME directories.
//! Interleave repeated runs; shared-workstation timings are exploratory only.

use std::hint::black_box;
use std::time::Instant;

use bytes::BytesMut;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use futures_util::{SinkExt, StreamExt};
use sockudo_ws::frame::{FrameParser, OpCode, encode_frame};
use sockudo_ws::{Config, Message, WebSocketStream};

fn bench_encoding(c: &mut Criterion) {
    let mut group = c.benchmark_group("client_mask_encode");
    for size in [8, 32, 125, 126, 256, 4096, 65536] {
        for (source_offset, destination_offset) in [(0, 0), (0, 6), (1, 13), (15, 1)] {
            let source = vec![0x42; size + 31];
            let start = source.as_ptr().align_offset(16) + source_offset;
            let payload = &source[start..start + size];
            let header_len = if size <= 125 {
                6
            } else if size <= 65535 {
                8
            } else {
                14
            };
            let mut output = BytesMut::with_capacity(size + 64);
            // Adjust the prefix for the payload's actual address, not the frame
            // header address; the allocator need not return an aligned pointer.
            let payload_offset = (output.as_ptr() as usize + header_len) & 15;
            let prefix_len = (destination_offset + 16 - payload_offset) & 15;
            output.resize(prefix_len, 0xa5);
            assert_eq!(payload.as_ptr() as usize & 15, source_offset);
            assert_eq!(
                (output.as_ptr() as usize + prefix_len + header_len) & 15,
                destination_offset
            );
            encode_frame(
                &mut output,
                OpCode::Binary,
                payload,
                true,
                Some([0x37, 0xfa, 0x21, 0x3d]),
            );
            let mut encoded = BytesMut::from(&output[prefix_len..]);
            let decoded = FrameParser::new(65536, true)
                .parse(&mut encoded)
                .unwrap()
                .unwrap();
            assert_eq!(decoded.payload.as_ref(), payload);
            assert!(encoded.is_empty());
            assert!(output[..prefix_len].iter().all(|&byte| byte == 0xa5));
            drop(decoded);
            group.throughput(Throughput::Bytes(size as u64));
            group.bench_function(
                BenchmarkId::new(
                    format!("src_{source_offset}_dst_{destination_offset}"),
                    size,
                ),
                |b| {
                    b.iter(|| {
                        output.truncate(prefix_len);
                        encode_frame(
                            black_box(&mut output),
                            OpCode::Binary,
                            black_box(payload),
                            true,
                            Some(black_box([0x37, 0xfa, 0x21, 0x3d])),
                        );
                        black_box(&output);
                    });
                },
            );
        }
    }
    group.finish();
}

fn bench_tcp(c: &mut Criterion) {
    const MESSAGES: usize = 1024;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let mut group = c.benchmark_group("client_mask_tcp");
    for size in [32, 256, 4096] {
        let message = Message::binary(vec![0x42; size]);
        for batch in [1, 16] {
            group.throughput(Throughput::Elements(MESSAGES as u64));
            group.bench_function(BenchmarkId::new(format!("batch_{batch}"), size), |b| {
                b.iter_custom(|iterations| {
                    runtime.block_on(async {
                        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                        let sender = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
                            .await
                            .unwrap();
                        let (receiver, _) = listener.accept().await.unwrap();
                        sender.set_nodelay(true).unwrap();
                        receiver.set_nodelay(true).unwrap();
                        let config = Config::builder().auto_ping(false).idle_timeout(0).build();
                        let mut sender = WebSocketStream::client(sender, config.clone());
                        let mut receiver = WebSocketStream::server(receiver, config);
                        let expected = message.clone();
                        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                        let peer = tokio::spawn(async move {
                            assert_eq!(
                                receiver.next().await.unwrap().unwrap().as_bytes(),
                                expected.as_bytes()
                            );
                            ready_tx.send(()).unwrap();
                            for _ in 0..iterations {
                                for _ in 0..MESSAGES {
                                    black_box(receiver.next().await.unwrap().unwrap());
                                }
                            }
                        });
                        // Validate client masking and server decoding before timing.
                        sender.send(message.clone()).await.unwrap();
                        ready_rx.await.unwrap();
                        let start = Instant::now();
                        for _ in 0..iterations {
                            if batch == 1 {
                                for _ in 0..MESSAGES {
                                    sender.send(message.clone()).await.unwrap();
                                }
                            } else {
                                for _ in 0..MESSAGES / batch {
                                    for _ in 0..batch {
                                        sender.feed(message.clone()).await.unwrap();
                                    }
                                    sender.flush().await.unwrap();
                                }
                            }
                        }
                        // Include final delivery: OS acceptance alone can hide a
                        // trailing receive backlog. One iteration is 1024 messages.
                        peer.await.unwrap();
                        start.elapsed()
                    })
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, bench_encoding, bench_tcp);
criterion_main!(benches);
