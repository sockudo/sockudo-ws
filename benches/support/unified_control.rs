//! Unified feed/receive cycles with automatic Pongs on a bounded duplex transport.
//!
//! One iteration sends 1,024 64-byte application messages. The peer sends a Ping
//! before each batch and validates all application bytes and the matching Pong
//! before sending the next Ping. This closed-loop fixture deliberately prevents
//! Pong coalescing, exposing the cost of control-induced flushes. It is not a
//! per-message latency distribution or a TCP throughput measurement.
//!
//! Both sides run on one current-thread runtime. Setup is outside the timer;
//! feeds, receives, final flush and peer validation are inside. The split case
//! uses futures::split with sequential calls from the same task to isolate the
//! adapter cost; it does not claim cross-thread read/write parallelism.

use std::time::{Duration, Instant};

use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use sockudo_ws::{Config, Error, Message, WebSocketStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

const MESSAGES: usize = 1024;
const PAYLOAD: [u8; 64] = [7; 64];

async fn peer(mut io: DuplexStream, batches: usize, batch_size: usize) {
    for sequence in 0..batches {
        let tag = sequence as u8;
        // Masked client Ping; a zero mask keeps the wire oracle simple.
        io.write_all(&[0x89, 0x81, 0, 0, 0, 0, tag]).await.unwrap();
        let mut messages = 0;
        let mut pong = false;
        while messages < batch_size || !pong {
            let mut header = [0; 2];
            io.read_exact(&mut header).await.unwrap();
            match header {
                [0x82, 64] => {
                    let mut payload = [0; 64];
                    io.read_exact(&mut payload).await.unwrap();
                    assert_eq!(payload, PAYLOAD);
                    messages += 1;
                    assert!(messages <= batch_size);
                }
                [0x8a, 1] => {
                    assert!(!pong);
                    assert_eq!(io.read_u8().await.unwrap(), tag);
                    pong = true;
                }
                other => panic!("unexpected frame header {other:?}"),
            }
        }
    }
}

async fn measure<W>(
    mut ws: W,
    io: DuplexStream,
    count: usize,
    batch_size: usize,
    split: bool,
) -> Duration
where
    W: Sink<Message, Error = Error> + Stream<Item = Result<Message, Error>> + Unpin,
{
    let batches = count / batch_size;
    let message = Message::Binary(Bytes::from_static(&PAYLOAD));
    let peer = tokio::spawn(peer(io, batches, batch_size));
    let start = Instant::now();
    if split {
        let (mut sink, mut stream) = StreamExt::split(ws);
        for sequence in 0..batches {
            for _ in 0..batch_size {
                sink.feed(message.clone()).await.unwrap();
            }
            assert!(
                matches!(stream.next().await, Some(Ok(Message::Ping(data))) if data.as_ref() == [sequence as u8])
            );
        }
        sink.flush().await.unwrap();
        peer.await.unwrap();
    } else {
        for sequence in 0..batches {
            for _ in 0..batch_size {
                ws.feed(message.clone()).await.unwrap();
            }
            assert!(
                matches!(ws.next().await, Some(Ok(Message::Ping(data))) if data.as_ref() == [sequence as u8])
            );
        }
        ws.flush().await.unwrap();
        peer.await.unwrap();
    }
    start.elapsed()
}

pub fn bench_control(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut group = c.benchmark_group("transport/extended/unified_control");
    group.throughput(Throughput::Elements(MESSAGES as u64));
    for (stream, compressed) in [
        ("plain", false),
        #[cfg(feature = "permessage-deflate")]
        ("compressed_rsv1_clear", true),
    ] {
        for (adapter, split) in [("direct", false), ("futures_split", true)] {
            for batch_size in [1, 32] {
                group.bench_function(
                    BenchmarkId::new(format!("{stream}/{adapter}/ping_every"), batch_size),
                    |b| {
                        b.iter_custom(|iterations| {
                            let count = usize::try_from(iterations)
                                .unwrap()
                                .checked_mul(MESSAGES)
                                .unwrap();
                            let (socket, peer) = tokio::io::duplex(256);
                            let config = Config::builder().auto_ping(false).idle_timeout(0).build();
                            #[cfg(feature = "permessage-deflate")]
                            if compressed {
                                let ws = sockudo_ws::CompressedWebSocketStream::server(
                                    socket,
                                    config,
                                    sockudo_ws::DeflateConfig {
                                        compression_threshold: usize::MAX,
                                        ..Default::default()
                                    },
                                );
                                return runtime
                                    .block_on(measure(ws, peer, count, batch_size, split));
                            }
                            let _ = compressed;
                            let ws = WebSocketStream::server(socket, config);
                            runtime.block_on(measure(ws, peer, count, batch_size, split))
                        });
                    },
                );
            }
        }
    }
    group.finish();
}
