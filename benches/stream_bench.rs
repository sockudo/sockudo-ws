//! Stream API benchmarks: local completion cost, not per-message P99 latency.
//!
//! Each Criterion iteration represents 1,024 messages on one connection. Setup,
//! runtime entry, task placement, and the final peer join are outside the clock.
//! The sending peer validates bytes concurrently with the timed writer.

use std::hint::black_box;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use futures_util::{SinkExt, StreamExt};
use sockudo_ws::frame::{OpCode, encode_frame};
use sockudo_ws::protocol::{Protocol, Role};
use sockudo_ws::{Config, Message, WebSocketStream};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const MESSAGES_PER_ITERATION: usize = 1024;
const PAYLOAD: [u8; 32] = [0x41; 32];
const BATCH_SIZE: usize = 16;
// Fixed queue capacity for the data-only model; the production split writer
// now uses a shared sink instead of an application-message queue.
const PROTOTYPE_QUEUE_CAPACITY: usize = 32;

#[derive(Clone, Copy)]
enum Operation {
    ReadUnified,
    ReadSplit,
    SendUnified,
    BatchUnified,
    SendSplit,
    PrototypeDirect,
    PrototypeBatch,
}

impl Operation {
    fn is_read(self) -> bool {
        matches!(self, Self::ReadUnified | Self::ReadSplit)
    }
}

fn config(timing: &str) -> Config {
    match timing {
        "default" => Config::default(),
        "idle_only" => Config::builder().auto_ping(false).build(),
        "ping_only" => Config::builder().idle_timeout(0).build(),
        "off" => Config::builder().auto_ping(false).idle_timeout(0).build(),
        _ => unreachable!(),
    }
}

async fn measure<S>(
    socket: S,
    mut peer: S,
    operation: Operation,
    config: Config,
    count: usize,
    payload: &'static [u8],
) -> Duration
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut frame = BytesMut::new();
    let mask = operation.is_read().then_some([1, 2, 3, 4]);
    encode_frame(&mut frame, OpCode::Binary, payload, true, mask);
    let frame_len = frame.len();
    let peer_task = if operation.is_read() {
        let mut wire = BytesMut::new();
        for _ in 0..MESSAGES_PER_ITERATION {
            wire.extend_from_slice(&frame);
        }
        tokio::spawn(async move {
            for _ in 0..count / MESSAGES_PER_ITERATION {
                peer.write_all(&wire).await.unwrap();
            }
            // Keep the peer open until the measured reader has consumed all messages.
            (peer, count * frame_len)
        })
    } else {
        let mut wire = BytesMut::new();
        for _ in 0..MESSAGES_PER_ITERATION {
            wire.extend_from_slice(&frame);
        }
        let mut buf = vec![0; wire.len()];
        tokio::spawn(async move {
            for _ in 0..count / MESSAGES_PER_ITERATION {
                peer.read_exact(&mut buf).await.unwrap();
                assert_eq!(buf.as_slice(), wire.as_ref());
            }
            (peer, count * frame_len)
        })
    };

    let (max_frame_size, max_message_size, write_capacity) = (
        config.max_frame_size,
        config.max_message_size,
        config.write_buffer_size,
    );
    let mut ws = WebSocketStream::server(socket, config);
    let message = Message::Binary(Bytes::copy_from_slice(payload));
    let elapsed = match operation {
        Operation::ReadUnified => {
            let mut last = None;
            let start = Instant::now();
            for _ in 0..count {
                last = Some(black_box(ws.next().await.unwrap().unwrap()));
            }
            let elapsed = start.elapsed();
            assert_eq!(last.unwrap().as_bytes(), payload);
            elapsed
        }
        Operation::ReadSplit => {
            let (mut reader, writer) = ws.split();
            let mut last = None;
            let start = Instant::now();
            for _ in 0..count {
                last = Some(black_box(reader.next().await.unwrap().unwrap()));
            }
            let elapsed = start.elapsed();
            assert_eq!(last.unwrap().as_bytes(), payload);
            drop(reader);
            drop(writer);
            let (_, bytes) = peer_task.await.unwrap();
            assert_eq!(bytes, count * frame_len);
            return elapsed;
        }
        Operation::SendUnified => {
            let start = Instant::now();
            for _ in 0..count {
                ws.send(message.clone()).await.unwrap();
            }
            start.elapsed()
        }
        Operation::BatchUnified => {
            let start = Instant::now();
            for _ in 0..count / BATCH_SIZE {
                for _ in 0..BATCH_SIZE {
                    ws.feed(message.clone()).await.unwrap();
                }
                ws.flush().await.unwrap();
            }
            start.elapsed()
        }
        // These data-only models intentionally omit production control ownership.
        Operation::PrototypeDirect => {
            let (reader_half, mut writer) = tokio::io::split(ws.into_inner());
            let mut protocol = Protocol::new(Role::Server, max_frame_size, max_message_size);
            let mut buffer = BytesMut::with_capacity(write_capacity);
            let start = Instant::now();
            for _ in 0..count {
                let message = message.clone();
                buffer.clear();
                protocol.encode_message(&message, &mut buffer).unwrap();
                writer.write_all(&buffer).await.unwrap();
                writer.flush().await.unwrap();
            }
            let elapsed = start.elapsed();
            drop(writer);
            drop(reader_half);
            let (_, bytes) = peer_task.await.unwrap();
            assert_eq!(bytes, count * frame_len);
            return elapsed;
        }
        Operation::PrototypeBatch => {
            let (reader_half, mut writer) = tokio::io::split(ws.into_inner());
            let (batch_tx, mut batch_rx) = tokio::sync::mpsc::channel::<(
                Vec<Message>,
                tokio::sync::oneshot::Sender<()>,
            )>(PROTOTYPE_QUEUE_CAPACITY);
            let driver = tokio::spawn(async move {
                let mut protocol = Protocol::new(Role::Server, max_frame_size, max_message_size);
                let mut buffer = BytesMut::with_capacity(write_capacity);
                while let Some((messages, completion)) = batch_rx.recv().await {
                    buffer.clear();
                    for message in messages {
                        protocol.encode_message(&message, &mut buffer).unwrap();
                    }
                    writer.write_all(&buffer).await.unwrap();
                    writer.flush().await.unwrap();
                    completion.send(()).unwrap();
                }
            });
            let start = Instant::now();
            for _ in 0..count / BATCH_SIZE {
                let messages = vec![message.clone(); BATCH_SIZE];
                let (completion, completed) = tokio::sync::oneshot::channel();
                batch_tx.send((messages, completion)).await.unwrap();
                completed.await.unwrap();
            }
            let elapsed = start.elapsed();
            drop(batch_tx);
            driver.await.unwrap();
            drop(reader_half);
            let (_, bytes) = peer_task.await.unwrap();
            assert_eq!(bytes, count * frame_len);
            return elapsed;
        }
        Operation::SendSplit => {
            let (reader, mut writer) = ws.split();
            let start = Instant::now();
            for _ in 0..count {
                writer.send(message.clone()).await.unwrap();
            }
            let elapsed = start.elapsed();
            drop(writer);
            drop(reader);
            let (_, bytes) = peer_task.await.unwrap();
            assert_eq!(bytes, count * frame_len);
            return elapsed;
        }
    };
    drop(ws);
    let (_, bytes) = peer_task.await.unwrap();
    assert_eq!(bytes, count * frame_len);
    elapsed
}

async fn sample(
    operation: Operation,
    config: Config,
    count: usize,
    tcp: bool,
    payload: &'static [u8],
) -> Duration {
    if tcp {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, accepted) = tokio::join!(
            TcpStream::connect(listener.local_addr().unwrap()),
            listener.accept(),
        );
        let client = client.unwrap();
        let (server, _) = accepted.unwrap();
        client.set_nodelay(true).unwrap();
        server.set_nodelay(true).unwrap();
        measure(server, client, operation, config, count, payload).await
    } else {
        let (peer, socket) = tokio::io::duplex(1 << 20);
        measure(socket, peer, operation, config, count, payload).await
    }
}

fn bench_stream(c: &mut Criterion) {
    for (placement, workers, spawn_caller) in [
        ("current_thread", 0, false),
        ("worker_1", 1, true),
        ("worker_4", 4, true),
        ("caller_4", 4, false),
    ] {
        let runtime = if workers == 0 {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
        } else {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(workers)
                .enable_all()
                .build()
                .unwrap()
        };
        for (transport, tcp) in [("duplex", false), ("tcp", true)] {
            let mut group = c.benchmark_group(format!("stream/{transport}/{placement}"));
            group.throughput(Throughput::Elements(MESSAGES_PER_ITERATION as u64));
            for (name, operation) in [
                ("read_unified", Operation::ReadUnified),
                ("read_split", Operation::ReadSplit),
                ("send_unified", Operation::SendUnified),
                ("feed_16", Operation::BatchUnified),
                ("send_split", Operation::SendSplit),
                ("prototype_direct", Operation::PrototypeDirect),
                ("prototype_batch_16", Operation::PrototypeBatch),
            ] {
                let timings: &[&str] = if operation.is_read() {
                    &["default", "idle_only", "ping_only", "off"]
                } else {
                    &["off"]
                };
                for &timing in timings {
                    group.bench_function(BenchmarkId::new(name, timing), |b| {
                        b.iter_custom(|iterations| {
                            let count = usize::try_from(iterations)
                                .unwrap()
                                .checked_mul(MESSAGES_PER_ITERATION)
                                .unwrap();
                            let future = sample(operation, config(timing), count, tcp, &PAYLOAD);
                            if spawn_caller {
                                runtime.block_on(async { tokio::spawn(future).await.unwrap() })
                            } else {
                                runtime.block_on(future)
                            }
                        });
                    });
                }
            }
            group.finish();
        }
    }
}

fn bench_masked_receive(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for (transport, tcp) in [("duplex", false), ("tcp", true)] {
        let mut group = c.benchmark_group(format!("masked_receive/{transport}"));
        group.throughput(Throughput::Elements(MESSAGES_PER_ITERATION as u64));
        for payload in [&[0x41; 125][..], &[0x41; 256], &[0x41; 1024]] {
            for (name, operation) in [
                ("unified", Operation::ReadUnified),
                ("split", Operation::ReadSplit),
            ] {
                group.bench_function(BenchmarkId::new(name, payload.len()), |b| {
                    b.iter_custom(|iterations| {
                        let count = usize::try_from(iterations)
                            .unwrap()
                            .checked_mul(MESSAGES_PER_ITERATION)
                            .unwrap();
                        runtime.block_on(sample(operation, config("off"), count, tcp, payload))
                    });
                });
            }
        }
        group.finish();
    }
}

criterion_group!(benches, bench_stream, bench_masked_receive);
criterion_main!(benches);
