//! TCP loopback send completion and peer delivery latency for 32B unmasked Binary messages.
//! Burst 0 is saturated; other bursts offer 1,000 messages/second per connection.
//! Select one case with --case workers mode connections burst count timing.
//! Every send awaits local completion; peer receipt is measured independently, without an ACK.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use futures_util::{SinkExt, StreamExt};
use sockudo_ws::{Config, Message, WebSocketStream};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Barrier;
use tokio_tungstenite::tungstenite::{Message as PeerMessage, protocol::Role};

const WARMUP_MESSAGES: usize = 256;

async fn connection(
    ws: WebSocketStream<TcpStream>,
    peer: TcpStream,
    split: bool,
    count: usize,
    burst: usize,
    ready: Arc<Barrier>,
    epoch: Instant,
) -> (Vec<u64>, Delivery) {
    let (peer_ready, warmed_up) = tokio::sync::oneshot::channel();
    let peer_task = tokio::spawn(async move {
        let mut peer =
            tokio_tungstenite::WebSocketStream::from_raw_socket(peer, Role::Client, None).await;
        for _ in 0..WARMUP_MESSAGES {
            let PeerMessage::Binary(payload) = peer.next().await.unwrap().unwrap() else {
                panic!("expected Binary warm-up message");
            };
            assert_eq!(payload.as_ref(), &[0; 32]);
        }
        peer_ready.send(()).unwrap();
        let mut samples = Delivery {
            latency_ns: Vec::with_capacity(count),
            scheduled_ns: Vec::with_capacity(count),
            sender_late_ns: Vec::with_capacity(count),
            first_due_ns: 0,
            last_received_ns: 0,
        };
        for sequence in 0..count {
            let PeerMessage::Binary(payload) = peer.next().await.unwrap().unwrap() else {
                panic!("expected Binary message");
            };
            let now = u64::try_from(epoch.elapsed().as_nanos()).unwrap();
            assert_eq!(payload.len(), 32);
            assert_eq!(&payload[24..], &[0; 8]);
            assert_eq!(
                u64::from_le_bytes(payload[..8].try_into().unwrap()),
                sequence as u64
            );
            let sent = u64::from_le_bytes(payload[8..16].try_into().unwrap());
            let due = u64::from_le_bytes(payload[16..24].try_into().unwrap());
            samples.latency_ns.push(now.checked_sub(sent).unwrap());
            samples.scheduled_ns.push(now.checked_sub(due).unwrap());
            samples.sender_late_ns.push(sent.checked_sub(due).unwrap());
            if sequence == 0 {
                samples.first_due_ns = due;
            }
            samples.last_received_ns = now;
        }
        // Keep the TCP peer alive until all local send completions have been collected.
        (samples, peer)
    });
    let send_ns = if split {
        let (_reader, mut writer) = ws.split();
        send_messages(&mut writer, count, burst, ready, epoch, warmed_up).await
    } else {
        let mut ws = ws;
        send_messages(&mut ws, count, burst, ready, epoch, warmed_up).await
    };
    let (delivery, _peer) = peer_task.await.unwrap();
    (send_ns, delivery)
}

async fn send_messages(
    writer: &mut impl SendMessage,
    count: usize,
    burst: usize,
    ready: Arc<Barrier>,
    epoch: Instant,
    warmed_up: tokio::sync::oneshot::Receiver<()>,
) -> Vec<u64> {
    let warmup = Message::Binary(bytes::Bytes::from_static(&[0; 32]));
    for _ in 0..WARMUP_MESSAGES {
        writer.send_message(warmup.clone()).await;
    }
    warmed_up.await.unwrap();
    let mut samples = Vec::with_capacity(count);
    ready.wait().await;
    let start = Instant::now() + Duration::from_millis(50);
    tokio::time::sleep_until(start.into()).await;
    for sequence in 0..count {
        let due = if let Some(batch) = sequence.checked_div(burst) {
            let due = start + Duration::from_millis((batch * burst) as u64);
            if sequence.is_multiple_of(burst) {
                tokio::time::sleep_until(due.into()).await;
            }
            due
        } else {
            // Saturated traffic has no independent arrival schedule.
            Instant::now()
        };
        let mut payload = BytesMut::zeroed(32);
        payload[..8].copy_from_slice(&(sequence as u64).to_le_bytes());
        let due_ns = u64::try_from(due.duration_since(epoch).as_nanos()).unwrap();
        payload[16..24].copy_from_slice(&due_ns.to_le_bytes());
        let sent_ns = u64::try_from(epoch.elapsed().as_nanos()).unwrap();
        payload[8..16].copy_from_slice(&sent_ns.to_le_bytes());
        let message = Message::Binary(payload.freeze());
        let begin = Instant::now();
        writer.send_message(message).await;
        let elapsed = u64::try_from(begin.elapsed().as_nanos()).unwrap();
        samples.push(elapsed);
    }
    samples
}

// SplitWriter does not implement Sink; static dispatch shares the measurement loop.
trait SendMessage {
    fn send_message(&mut self, message: Message) -> impl Future<Output = ()> + Send;
}

impl SendMessage for WebSocketStream<TcpStream> {
    async fn send_message(&mut self, message: Message) {
        self.send(message).await.unwrap();
    }
}

impl SendMessage for sockudo_ws::SplitWriter<TcpStream> {
    async fn send_message(&mut self, message: Message) {
        self.send(message).await.unwrap();
    }
}

struct Delivery {
    latency_ns: Vec<u64>,
    scheduled_ns: Vec<u64>,
    sender_late_ns: Vec<u64>,
    first_due_ns: u64,
    last_received_ns: u64,
}

fn percentile(sorted: &[u64], percent: usize) -> f64 {
    percentile_ns(sorted, percent) as f64 / 1_000.0
}

fn percentile_ns(sorted: &[u64], percent: usize) -> u64 {
    sorted[(sorted.len() * percent).div_ceil(100) - 1]
}

fn percentile_spread(sorted: &mut [u64]) -> u64 {
    sorted.sort_unstable();
    percentile_ns(sorted, 99) - percentile_ns(sorted, 1)
}

async fn run(
    workers: usize,
    mode: &str,
    connections: usize,
    burst: usize,
    count: usize,
    timing: &str,
) {
    assert!(matches!(mode, "unified" | "split"));
    assert!(connections > 0 && count > 0);
    let config = match timing {
        "default" => Config::default(),
        "off" => Config::builder().auto_ping(false).idle_timeout(0).build(),
        _ => panic!("expected default or off timing"),
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut sockets = Vec::new();
    for _ in 0..connections {
        let peer = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        socket.set_nodelay(true).unwrap();
        peer.set_nodelay(true).unwrap();
        sockets.push((WebSocketStream::server(socket, config.clone()), peer));
    }
    let epoch = Instant::now();
    let ready = Arc::new(Barrier::new(connections));
    let tasks: Vec<_> = sockets
        .into_iter()
        .map(|(ws, peer)| {
            tokio::spawn(connection(
                ws,
                peer,
                mode == "split",
                count,
                burst,
                ready.clone(),
                epoch,
            ))
        })
        .collect();
    let mut send = Vec::new();
    let mut send_burst_first = Vec::new();
    let mut send_burst_nonfirst = Vec::new();
    let mut delivery = Vec::new();
    let mut scheduled = Vec::new();
    let mut sender_late = Vec::new();
    let mut scheduled_spreads = Vec::with_capacity(connections);
    let mut sender_late_spreads = Vec::with_capacity(connections);
    let mut first_due = u64::MAX;
    let mut last_received = 0;
    for task in tasks {
        let (send_samples, mut received) = task.await.unwrap();
        if burst != 0 {
            for (sequence, sample) in send_samples.iter().copied().enumerate() {
                if sequence.is_multiple_of(burst) {
                    send_burst_first.push(sample);
                } else {
                    send_burst_nonfirst.push(sample);
                }
            }
        }
        send.extend(send_samples);
        if burst != 0 {
            scheduled_spreads.push(percentile_spread(&mut received.scheduled_ns));
            sender_late_spreads.push(percentile_spread(&mut received.sender_late_ns));
        }
        delivery.extend(received.latency_ns);
        scheduled.extend(received.scheduled_ns);
        sender_late.extend(received.sender_late_ns);
        first_due = first_due.min(received.first_due_ns);
        last_received = last_received.max(received.last_received_ns);
    }
    let messages = count.checked_mul(connections).unwrap();
    assert_eq!(send.len(), messages);
    assert_eq!(delivery.len(), messages);
    send.sort_unstable();
    send_burst_first.sort_unstable();
    send_burst_nonfirst.sort_unstable();
    delivery.sort_unstable();
    scheduled.sort_unstable();
    sender_late.sort_unstable();
    let achieved = messages as f64 * 1e9 / last_received.checked_sub(first_due).unwrap() as f64;
    let scheduled_p99 = if burst == 0 {
        String::new()
    } else {
        format!("{:.3}", percentile(&scheduled, 99))
    };
    let sender_late_p99 = if burst == 0 {
        String::new()
    } else {
        format!("{:.3}", percentile(&sender_late, 99))
    };
    let scheduled_p99_p1_max = if burst == 0 {
        String::new()
    } else {
        format!(
            "{:.3}",
            scheduled_spreads.into_iter().max().unwrap() as f64 / 1_000.0
        )
    };
    let sender_late_p99_p1_max = if burst == 0 {
        String::new()
    } else {
        format!(
            "{:.3}",
            sender_late_spreads.into_iter().max().unwrap() as f64 / 1_000.0
        )
    };
    let send_burst_first_p50 = format_percentile(&send_burst_first, 50);
    let send_burst_first_p90 = format_percentile(&send_burst_first, 90);
    let send_burst_nonfirst_p50 = format_percentile(&send_burst_nonfirst, 50);
    let send_burst_nonfirst_p95 = format_percentile(&send_burst_nonfirst, 95);
    let send_burst_nonfirst_p99 = format_percentile(&send_burst_nonfirst, 99);
    println!(
        "{workers},{mode},{connections},{burst},{timing},{messages},{achieved:.1},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{scheduled_p99},{sender_late_p99},{scheduled_p99_p1_max},{sender_late_p99_p1_max},{send_burst_first_p50},{send_burst_first_p90},{send_burst_nonfirst_p50},{send_burst_nonfirst_p95},{send_burst_nonfirst_p99}",
        percentile(&send, 50),
        percentile(&send, 95),
        percentile(&send, 99),
        percentile(&delivery, 50),
        percentile(&delivery, 95),
        percentile(&delivery, 99),
    );
}

fn format_percentile(sorted: &[u64], percent: usize) -> String {
    if sorted.is_empty() {
        String::new()
    } else {
        format!("{:.3}", percentile(sorted, percent))
    }
}

fn main() {
    // Cargo passes --bench to harness-free benchmark executables.
    let args: Vec<_> = std::env::args()
        .skip(1)
        .filter(|arg| arg != "--bench")
        .collect();
    println!(
        "workers,mode,connections,burst,timing,messages,achieved_msg_s,send_p50_us,send_p95_us,send_p99_us,delivery_p50_us,delivery_p95_us,delivery_p99_us,scheduled_p99_us,sender_late_p99_us,scheduled_p99_p1_max_us,sender_late_p99_p1_max_us,send_burst_first_p50_us,send_burst_first_p90_us,send_burst_nonfirst_p50_us,send_burst_nonfirst_p95_us,send_burst_nonfirst_p99_us"
    );
    if args.first().is_some_and(|arg| arg == "--case") {
        assert_eq!(
            args.len(),
            7,
            "--case workers mode connections burst count timing"
        );
        let workers = args[1].parse().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(run(
            workers,
            &args[2],
            args[3].parse().unwrap(),
            args[4].parse().unwrap(),
            args[5].parse().unwrap(),
            &args[6],
        ));
        return;
    }
    let smoke = args.is_empty() || args == ["--test"];
    assert!(
        smoke || args == ["--matrix"],
        "expected --case, --matrix, or no arguments"
    );
    for workers in [1, 4] {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            for mode in ["unified", "split"] {
                for &connections in if smoke { &[1][..] } else { &[1, 16][..] } {
                    for &burst in if smoke { &[0, 64][..] } else { &[0, 1, 64][..] } {
                        let count = if smoke {
                            128
                        } else if burst == 0 {
                            20_000
                        } else {
                            2048
                        };
                        run(workers, mode, connections, burst, count, "default").await;
                    }
                }
            }
        });
    }
}
