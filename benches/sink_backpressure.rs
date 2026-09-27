//! Queued-write backpressure through the unified Tokio Sink API.
//!
//! Run with `cargo bench --no-default-features --features
//! tokio-runtime,permessage-deflate --bench sink_backpressure`.
//! Each Criterion iteration is 1,024 messages on a current-thread runtime.
//! Timing covers the sender loop and final flush, including any peer reads and
//! byte validation scheduled during that interval. Runtime/stream/input setup
//! and the final peer join are outside the timer. Results are batch costs, not
//! individual-message percentiles or TCP latency. The compressed wrapper uses
//! a disabled compression threshold to isolate its Sink implementation.
//! Pending-inbound cases leave one parsed input queued during writes, then
//! consume it and explicitly flush before ending the timer. On older revisions
//! send/flush may return without draining in this state: those cases compare
//! batching policies, not equivalent per-call completion guarantees.

use std::hint::black_box;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use sockudo_ws::frame::{OpCode, encode_frame};
use sockudo_ws::{
    CompressedWebSocketStream, Config, DeflateConfig, Error, Message, WebSocketStream,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

const MESSAGES_PER_ITERATION: usize = 1_024;

#[derive(Clone, Copy)]
enum Operation {
    Send,
    FeedDefaultThreshold,
    FeedUnbounded,
    FeedEveryMessage,
    FeedBatchingDisabled,
    SendPendingInbound,
    FeedPendingInbound,
}

impl Operation {
    fn name(self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::FeedDefaultThreshold => "feed_default_threshold",
            Self::FeedUnbounded => "feed_max_unbounded",
            Self::FeedEveryMessage => "feed_zero_threshold",
            Self::FeedBatchingDisabled => "feed_batching_disabled",
            Self::SendPendingInbound => "send_pending_inbound",
            Self::FeedPendingInbound => "feed_pending_inbound",
        }
    }

    fn config(self) -> Config {
        let builder = Config::builder().auto_ping(false).idle_timeout(0);
        match self {
            Self::FeedUnbounded => builder.max_backpressure(usize::MAX).build(),
            Self::FeedEveryMessage => builder.max_backpressure(0).build(),
            Self::FeedBatchingDisabled => builder.write_coalescing(false).build(),
            Self::Send
            | Self::FeedDefaultThreshold
            | Self::SendPendingInbound
            | Self::FeedPendingInbound => builder.build(),
        }
    }
}

fn payload(len: usize) -> Bytes {
    Bytes::from((0..len).map(|index| index as u8).collect::<Vec<_>>())
}

fn expected_wire(payload: &[u8]) -> BytesMut {
    let mut frame = BytesMut::new();
    encode_frame(&mut frame, OpCode::Binary, payload, true, None);
    let mut wire = BytesMut::with_capacity(frame.len() * MESSAGES_PER_ITERATION);
    for _ in 0..MESSAGES_PER_ITERATION {
        wire.extend_from_slice(&frame);
    }
    wire
}

async fn measure<W>(
    mut writer: W,
    mut peer: DuplexStream,
    operation: Operation,
    payload: Bytes,
    count: usize,
) -> Duration
where
    W: Sink<Message, Error = Error> + Stream<Item = Result<Message, Error>> + Unpin,
{
    let pending_inbound = matches!(
        operation,
        Operation::SendPendingInbound | Operation::FeedPendingInbound
    );
    if pending_inbound {
        let mut inbound = BytesMut::new();
        for value in [1, 2] {
            encode_frame(&mut inbound, OpCode::Binary, &[value], true, Some([3; 4]));
        }
        peer.write_all(&inbound).await.unwrap();
        assert_eq!(writer.next().await.unwrap().unwrap().as_bytes(), &[1]);
    }
    let expected = expected_wire(&payload);
    let expected_len = expected.len();
    let repetitions = count / MESSAGES_PER_ITERATION;
    let peer_task = tokio::spawn(async move {
        let mut received = vec![0; expected_len];
        let mut checksum = 0u64;
        for _ in 0..repetitions {
            peer.read_exact(&mut received).await.unwrap();
            assert_eq!(received.as_slice(), expected.as_ref());
            checksum = checksum.wrapping_add(received[expected_len - 1] as u64);
        }
        checksum
    });
    let message = Message::Binary(payload);

    let started = Instant::now();
    match operation {
        Operation::Send | Operation::SendPendingInbound => {
            for _ in 0..count {
                writer.send(message.clone()).await.unwrap();
            }
        }
        Operation::FeedDefaultThreshold
        | Operation::FeedUnbounded
        | Operation::FeedEveryMessage
        | Operation::FeedBatchingDisabled
        | Operation::FeedPendingInbound => {
            for _ in 0..repetitions {
                for _ in 0..MESSAGES_PER_ITERATION {
                    writer.feed(message.clone()).await.unwrap();
                }
                writer.flush().await.unwrap();
            }
        }
    }
    if pending_inbound {
        // Drain the final batch on both revisions, including old implementations
        // whose explicit flush returned early while this input was queued.
        assert_eq!(writer.next().await.unwrap().unwrap().as_bytes(), &[2]);
        writer.flush().await.unwrap();
    }
    let elapsed = started.elapsed();
    drop(writer);
    let checksum = peer_task.await.unwrap();
    assert_eq!(
        checksum,
        repetitions as u64 * expected_wire(message.as_bytes()).last().copied().unwrap() as u64
    );
    black_box(checksum);
    elapsed
}

async fn sample(
    compressed: bool,
    operation: Operation,
    payload_len: usize,
    count: usize,
) -> Duration {
    let (socket, peer) = tokio::io::duplex(1 << 20);
    let payload = payload(payload_len);
    if compressed {
        let writer = CompressedWebSocketStream::server(
            socket,
            operation.config(),
            DeflateConfig {
                compression_threshold: usize::MAX,
                ..Default::default()
            },
        );
        measure(writer, peer, operation, payload, count).await
    } else {
        let writer = WebSocketStream::server(socket, operation.config());
        measure(writer, peer, operation, payload, count).await
    }
}

fn bench_backpressure(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut group = c.benchmark_group("backpressure");
    group.throughput(Throughput::Elements(MESSAGES_PER_ITERATION as u64));
    for (stream, compressed) in [("plain", false), ("compressed", true)] {
        for operation in [
            Operation::Send,
            Operation::FeedDefaultThreshold,
            Operation::FeedUnbounded,
            Operation::FeedEveryMessage,
            Operation::FeedBatchingDisabled,
            Operation::SendPendingInbound,
            Operation::FeedPendingInbound,
        ] {
            for payload_len in [32, 4096] {
                group.bench_function(
                    BenchmarkId::new(format!("{stream}/{}", operation.name()), payload_len),
                    |b| {
                        b.iter_custom(|iterations| {
                            let count = usize::try_from(iterations)
                                .unwrap()
                                .checked_mul(MESSAGES_PER_ITERATION)
                                .unwrap();
                            runtime.block_on(sample(compressed, operation, payload_len, count))
                        });
                    },
                );
            }
        }
    }
    group.finish();
}

criterion_group!(benches, bench_backpressure);
criterion_main!(benches);
