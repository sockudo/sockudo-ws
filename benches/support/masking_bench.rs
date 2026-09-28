//! Copy-inclusive masked frame parsing and TCP server receive benchmarks.
//!
//! Run identical sources and features on an idle host for each revision, with
//! separate CARGO_TARGET_DIR and CRITERION_HOME directories. The kernel exercises
//! the host architecture: NEON results require an aarch64 host. Compare repeated,
//! interleaved runs; timings on a shared workstation are exploratory only.

use std::hint::black_box;
#[cfg(feature = "tokio-runtime")]
use std::time::Instant;

use bytes::BytesMut;
use criterion::{BenchmarkId, Criterion, Throughput};
#[cfg(feature = "tokio-runtime")]
use futures_util::StreamExt;
use sockudo_ws::frame::{FrameParser, OpCode, encode_frame};
#[cfg(feature = "tokio-runtime")]
use sockudo_ws::{Config, WebSocketStream};
#[cfg(feature = "tokio-runtime")]
use tokio::io::AsyncWriteExt;

pub fn bench_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("kernels/extended/parse_copy");
    for size in [8, 32, 64, 125, 256, 1024, 4096] {
        let payload = vec![0x42; size];
        let mut wire = BytesMut::new();
        encode_frame(
            &mut wire,
            OpCode::Binary,
            &payload,
            true,
            Some([0x37, 0xfa, 0x21, 0x3d]),
        );
        let mut parser = FrameParser::new(65536, true);
        let mut check = wire.clone();
        assert_eq!(parser.parse(&mut check).unwrap().unwrap().payload, payload);
        assert!(check.is_empty());
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_function(BenchmarkId::from_parameter(size), |b| {
            b.iter(|| {
                // Include input allocation/copy and frame destruction. Unlike
                // the kernels suite's batched parser case, this times input preparation.
                let mut input = BytesMut::from(wire.as_ref());
                black_box(parser.parse(black_box(&mut input)).unwrap().unwrap());
            });
        });
    }
    group.finish();
}

#[cfg(feature = "tokio-runtime")]
pub fn bench_tcp(c: &mut Criterion) {
    const BATCH: usize = 1024;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let mut group = c.benchmark_group("transport/extended/masked_tcp_receive");
    for size in [32, 256, 4096] {
        let payload = vec![0x42; size];
        let mut frame = BytesMut::new();
        encode_frame(
            &mut frame,
            OpCode::Binary,
            &payload,
            true,
            Some([0x37, 0xfa, 0x21, 0x3d]),
        );
        let wire = frame.repeat(BATCH);
        group.throughput(Throughput::Elements(BATCH as u64));
        for timers in [false, true] {
            let mode = if timers { "default" } else { "timers_off" };
            group.bench_function(BenchmarkId::new(mode, size), |b| {
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
                        let config = if timers {
                            Config::default()
                        } else {
                            Config::builder().auto_ping(false).idle_timeout(0).build()
                        };
                        let mut ws = WebSocketStream::server(receiver, config);
                        // Validate the real receive path before timing. Setup,
                        // validation and final sender join are outside the timer.
                        sender.write_all(&frame).await.unwrap();
                        assert_eq!(ws.next().await.unwrap().unwrap().as_bytes(), payload);
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
                            for _ in 0..BATCH {
                                black_box(ws.next().await.unwrap().unwrap());
                            }
                        }
                        let elapsed = start.elapsed();
                        producer.await.unwrap();
                        elapsed
                    })
                });
            });
        }
    }
    group.finish();
}
