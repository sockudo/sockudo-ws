//! Synthetic loopback delivery latency with paced and burst arrivals.
//! The independent tungstenite peer also completes two native Ping/Pong cycles.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use sockudo_ws::{Config, Message, WebSocketStream};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{Message as PeerMessage, protocol::Role};

struct Samples {
    delivery_ns: Vec<u64>,
    scheduled_ns: Vec<u64>,
    sender_late_ns: Vec<u64>,
    last_received_ns: u64,
}

async fn connection(
    ws: WebSocketStream<TcpStream>,
    peer: TcpStream,
    epoch: Instant,
    count: usize,
    burst: usize,
    split: bool,
) -> Samples {
    let peer_task = tokio::spawn(async move {
        let mut peer =
            tokio_tungstenite::WebSocketStream::from_raw_socket(peer, Role::Server, None).await;
        let mut sent = 0usize;
        let mut pings = 0usize;
        loop {
            let due = epoch + Duration::from_millis(((sent / burst) * burst) as u64);
            tokio::select! {
                incoming = peer.next() => {
                    match incoming.unwrap().unwrap() {
                        PeerMessage::Ping(_) => {
                            // Tungstenite queues the matching Pong; flush it before closing.
                            peer.flush().await.unwrap();
                            pings += 1;
                            if sent == count && pings == 2 {
                                peer.close(None).await.unwrap();
                                break;
                            }
                        }
                        other => panic!("unexpected peer input: {other:?}"),
                    }
                }
                _ = tokio::time::sleep_until(due), if sent < count => {
                    for _ in 0..burst.min(count - sent) {
                        let mut payload = [0u8; 32];
                        payload[..8].copy_from_slice(&(sent as u64).to_le_bytes());
                        payload[8..16].copy_from_slice(&(epoch.elapsed().as_nanos() as u64).to_le_bytes());
                        peer.send(PeerMessage::Binary(payload.to_vec().into())).await.unwrap();
                        sent += 1;
                    }
                }
            }
        }
        pings
    });

    let samples = if split {
        let (reader, _writer) = ws.split();
        receive(reader, epoch, count, burst).await
    } else {
        receive(ws, epoch, count, burst).await
    };
    assert_eq!(peer_task.await.unwrap(), 2);
    samples
}

async fn receive(
    mut reader: impl ReceiveMessage,
    epoch: Instant,
    count: usize,
    burst: usize,
) -> Samples {
    let mut samples = Samples {
        delivery_ns: Vec::with_capacity(count),
        scheduled_ns: Vec::with_capacity(count),
        sender_late_ns: Vec::with_capacity(count),
        last_received_ns: 0,
    };
    let mut pongs = 0usize;
    while let Some(message) = reader.receive_message().await {
        match message.unwrap() {
            Message::Binary(payload) => {
                let now = epoch.elapsed().as_nanos() as u64;
                let sequence = u64::from_le_bytes(payload[..8].try_into().unwrap()) as usize;
                assert_eq!(sequence, samples.delivery_ns.len());
                assert_eq!(payload.len(), 32);
                let sent = u64::from_le_bytes(payload[8..16].try_into().unwrap());
                let scheduled = ((sequence / burst) * burst) as u64 * 1_000_000;
                samples.delivery_ns.push(now.checked_sub(sent).unwrap());
                samples
                    .scheduled_ns
                    .push(now.checked_sub(scheduled).unwrap());
                samples
                    .sender_late_ns
                    .push(sent.checked_sub(scheduled).unwrap());
                samples.last_received_ns = now;
            }
            Message::Pong(_) => pongs += 1,
            Message::Close(_) => break,
            other => panic!("unexpected reader input: {other:?}"),
        }
    }
    assert_eq!(samples.delivery_ns.len(), count);
    assert_eq!(pongs, 2);
    samples
}

// SplitReader has an inherent next method rather than implementing Stream.
// Static dispatch keeps both APIs on the same measurement loop without boxing each future.
trait ReceiveMessage {
    fn receive_message(
        &mut self,
    ) -> impl Future<Output = Option<sockudo_ws::Result<Message>>> + Send;
}

impl ReceiveMessage for WebSocketStream<TcpStream> {
    async fn receive_message(&mut self) -> Option<sockudo_ws::Result<Message>> {
        self.next().await
    }
}

impl ReceiveMessage for sockudo_ws::SplitReader<TcpStream> {
    async fn receive_message(&mut self) -> Option<sockudo_ws::Result<Message>> {
        self.next().await
    }
}

fn percentile(sorted: &[u64], percent: usize) -> f64 {
    let index = (sorted.len() * percent).div_ceil(100) - 1;
    sorted[index] as f64 / 1_000.0
}

async fn run(workers: usize, split: bool, connections: usize, count: usize, burst: usize) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut sockets = Vec::new();
    for _ in 0..connections {
        let socket = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (peer, _) = listener.accept().await.unwrap();
        socket.set_nodelay(true).unwrap();
        peer.set_nodelay(true).unwrap();
        let config = Config::builder()
            .ping_interval(1)
            .pong_timeout(2)
            .idle_timeout(5)
            .build();
        sockets.push((WebSocketStream::client(socket, config), peer));
    }
    // Construct every connection before scheduling traffic, including cold clock calibration.
    let epoch = Instant::now() + Duration::from_millis(50);
    let tasks: Vec<_> = sockets
        .into_iter()
        .map(|(ws, peer)| tokio::spawn(connection(ws, peer, epoch, count, burst, split)))
        .collect();
    let mut delivery = Vec::new();
    let mut scheduled = Vec::new();
    let mut sender_late = Vec::new();
    let mut last_received = 0;
    for task in tasks {
        let samples = task.await.unwrap();
        delivery.extend(samples.delivery_ns);
        scheduled.extend(samples.scheduled_ns);
        sender_late.extend(samples.sender_late_ns);
        last_received = last_received.max(samples.last_received_ns);
    }
    delivery.sort_unstable();
    scheduled.sort_unstable();
    sender_late.sort_unstable();
    let mode = if split { "split" } else { "unified" };
    println!(
        "{workers},{mode},{connections},{burst},{},{:.1},{:.3},{:.3},{:.3},{:.3},{:.3}",
        count * connections,
        (count * connections) as f64 * 1e9 / last_received as f64,
        percentile(&delivery, 50),
        percentile(&delivery, 95),
        percentile(&delivery, 99),
        percentile(&scheduled, 99),
        percentile(&sender_late, 99),
    );
}

fn main() {
    let smoke = std::env::args().any(|arg| arg == "--test");
    println!(
        "workers,mode,connections,burst,messages,achieved_msg_s,delivery_p50_us,delivery_p95_us,delivery_p99_us,scheduled_p99_us,sender_late_p99_us"
    );
    for workers in [1, 4] {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            for split in [false, true] {
                if smoke {
                    run(workers, split, 1, 128, 64).await;
                } else {
                    for connections in [1, 16] {
                        for burst in [1, 64] {
                            run(workers, split, connections, 2048, burst).await;
                        }
                    }
                }
            }
        });
    }
}
