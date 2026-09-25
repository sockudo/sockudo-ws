//! Per-connection TCP delivery timelines for saturated or paced 32-byte messages.
//! Receiver placement changes both runtime ownership and the available worker count.
//! CSV connection rows: id, workers, count, rate, delivery_p99_ns, send_p99_ns,
//! late_p99_ns, first_sent_ns, last_received_ns, max_gap_ns, gap_before_ns, gap_after_ns,
//! scheduled_p99_ns, scheduled_p99_p1_ns, sender_late_p99_p1_ns.
//! Scheduling fields are empty for saturated traffic; all percentiles are per connection.
//! CSV slow rows: id, sequence, due_ns, sent_ns, completed_ns, received_ns, delivery_ns.
//! Default: one worker and connection, 128 messages at 1,000 messages/second, WS, tracing off.
//! Arguments: sender_workers receiver_workers connections count rate_per_connection.
//! Optional trailing arguments: ws|raw off|on (protocol and read/task tracing).
//! CSV trace rows: id, read_polls, task_polls, max_wake_to_poll_ns, max_task_poll_ns.
//! CSV task rows: id, first_wake_ns (0=no recorded wake), poll_start_ns, poll_end_ns.
//! CSV read rows: id, poll_start_ns, poll_end_ns, bytes, pending.
//! Trace event times include warmup; message first_sent_ns marks the measured phase.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use bytes::BytesMut;
use futures_util::{SinkExt, StreamExt};
use sockudo_ws::{Config, Message, WebSocketStream};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Barrier, oneshot};
use tokio_tungstenite::tungstenite::{Message as PeerMessage, protocol::Role};

const WARMUP_MESSAGES: usize = 256;

// One millisecond selects diagnostic events well below the observed second-long stalls.
const TRACE_EVENT_NS: u64 = 1_000_000;

struct WakeTrace {
    parent: Waker,
    epoch: Instant,
    first_wake_ns: AtomicU64,
}

impl Wake for WakeTrace {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let now = u64::try_from(self.epoch.elapsed().as_nanos()).unwrap();
        // Coalesced notifications retain the first wake until the next poll.
        let _ = self
            .first_wake_ns
            .compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed);
        self.parent.wake_by_ref();
    }
}

struct TaskPoll {
    wake_ns: u64,
    start_ns: u64,
    end_ns: u64,
}

struct ReadPoll {
    start_ns: u64,
    end_ns: u64,
    bytes: usize,
    pending: bool,
}

struct ReadTrace<const TRACE: bool> {
    inner: TcpStream,
    epoch: Instant,
    polls: Vec<ReadPoll>,
}

impl<const TRACE: bool> AsyncRead for ReadTrace<TRACE> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if !TRACE {
            return Pin::new(&mut self.inner).poll_read(cx, buf);
        }
        let start_ns = u64::try_from(self.epoch.elapsed().as_nanos()).unwrap();
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        let end_ns = u64::try_from(self.epoch.elapsed().as_nanos()).unwrap();
        self.polls.push(ReadPoll {
            start_ns,
            end_ns,
            bytes: buf.filled().len() - before,
            pending: result.is_pending(),
        });
        result
    }
}

impl<const TRACE: bool> AsyncWrite for ReadTrace<TRACE> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

enum Peer<const TRACE: bool> {
    Raw(BufReader<ReadTrace<TRACE>>),
    WebSocket(Box<tokio_tungstenite::WebSocketStream<ReadTrace<TRACE>>>),
}

impl<const TRACE: bool> Peer<TRACE> {
    async fn payload(&mut self) -> bytes::Bytes {
        match self {
            Self::Raw(peer) => {
                let mut frame = [0; 34];
                peer.read_exact(&mut frame).await.unwrap();
                assert_eq!(&frame[..2], &[0x82, 32]);
                bytes::Bytes::copy_from_slice(&frame[2..])
            }
            Self::WebSocket(peer) => {
                let PeerMessage::Binary(payload) = peer.next().await.unwrap().unwrap() else {
                    panic!("expected binary message");
                };
                payload
            }
        }
    }

    fn read_polls(&self) -> &[ReadPoll] {
        match self {
            Self::Raw(peer) => &peer.get_ref().polls,
            Self::WebSocket(peer) => &peer.get_ref().polls,
        }
    }
}

async fn receive_task<const RAW: bool, const TRACE: bool>(
    peer: std::net::TcpStream,
    count: usize,
    epoch: Instant,
    warmed_up: oneshot::Sender<()>,
) -> ((Received, Peer<TRACE>), Vec<TaskPoll>) {
    if !TRACE {
        return (
            receive::<RAW, TRACE>(peer, count, epoch, warmed_up).await,
            Vec::new(),
        );
    }
    let mut future = std::pin::pin!(receive::<RAW, TRACE>(peer, count, epoch, warmed_up));
    let mut wake: Option<Arc<WakeTrace>> = None;
    let mut polls = Vec::with_capacity(count);
    let result = std::future::poll_fn(|cx| {
        let wake = wake.get_or_insert_with(|| {
            Arc::new(WakeTrace {
                parent: cx.waker().clone(),
                epoch,
                first_wake_ns: AtomicU64::new(0),
            })
        });
        let wake_ns = wake.first_wake_ns.swap(0, Ordering::Relaxed);
        let start_ns = u64::try_from(epoch.elapsed().as_nanos()).unwrap();
        let waker = Waker::from(wake.clone());
        let result = future.as_mut().poll(&mut Context::from_waker(&waker));
        let end_ns = u64::try_from(epoch.elapsed().as_nanos()).unwrap();
        polls.push(TaskPoll {
            wake_ns,
            start_ns,
            end_ns,
        });
        result
    })
    .await;
    (result, polls)
}

struct Received {
    sent_ns: Vec<u64>,
    received_ns: Vec<u64>,
    due_ns: Vec<u64>,
}

async fn receive<const RAW: bool, const TRACE: bool>(
    peer: std::net::TcpStream,
    count: usize,
    epoch: Instant,
    warmed_up: oneshot::Sender<()>,
) -> (Received, Peer<TRACE>) {
    // Register the socket with the runtime that actually drives the receiver.
    let peer = TcpStream::from_std(peer).unwrap();
    let peer = ReadTrace::<TRACE> {
        inner: peer,
        epoch,
        polls: if TRACE {
            Vec::with_capacity(count)
        } else {
            Vec::new()
        },
    };
    let mut peer = if RAW {
        let capacity =
            tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default().read_buffer_size;
        Peer::Raw(BufReader::with_capacity(capacity, peer))
    } else {
        Peer::WebSocket(Box::new(
            tokio_tungstenite::WebSocketStream::from_raw_socket(peer, Role::Client, None).await,
        ))
    };
    for _ in 0..WARMUP_MESSAGES {
        assert_eq!(peer.payload().await.as_ref(), &[0; 32]);
    }
    let mut samples = Received {
        sent_ns: Vec::with_capacity(count),
        received_ns: Vec::with_capacity(count),
        due_ns: Vec::with_capacity(count),
    };
    warmed_up.send(()).unwrap();
    for sequence in 0..count {
        let payload = peer.payload().await;
        let received = u64::try_from(epoch.elapsed().as_nanos()).unwrap();
        assert_eq!(payload.len(), 32);
        assert_eq!(&payload[24..], &[0; 8]);
        assert_eq!(
            u64::from_le_bytes(payload[..8].try_into().unwrap()),
            sequence as u64
        );
        samples
            .sent_ns
            .push(u64::from_le_bytes(payload[8..16].try_into().unwrap()));
        samples
            .due_ns
            .push(u64::from_le_bytes(payload[16..24].try_into().unwrap()));
        samples.received_ns.push(received);
    }
    // Preserve the socket until the sender has collected all local completions.
    (samples, peer)
}

fn percentile(values: &[u64], percent: usize) -> u64 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted[(sorted.len() * percent).div_ceil(100) - 1]
}

async fn run<const RAW: bool, const TRACE: bool>(
    workers: usize,
    receiver: tokio::runtime::Handle,
    connections: usize,
    count: usize,
    rate: usize,
) {
    assert!(connections > 0 && count > 1);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut sockets = Vec::new();
    for _ in 0..connections {
        let peer = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        peer.set_nodelay(true).unwrap();
        socket.set_nodelay(true).unwrap();
        sockets.push((
            WebSocketStream::server(socket, Config::default()),
            peer.into_std().unwrap(),
        ));
    }
    let epoch = Instant::now();
    let ready = Arc::new(Barrier::new(connections));
    let mut tasks = Vec::new();
    for (id, (mut sender, peer)) in sockets.into_iter().enumerate() {
        let ready = ready.clone();
        let receiver = receiver.clone();
        tasks.push(tokio::spawn(async move {
            let (warmed_up, warmup_done) = oneshot::channel();
            let peer_task =
                receiver.spawn(receive_task::<RAW, TRACE>(peer, count, epoch, warmed_up));
            let warmup = Message::Binary(bytes::Bytes::from_static(&[0; 32]));
            for _ in 0..WARMUP_MESSAGES {
                if RAW {
                    let mut frame = [0; 34];
                    frame[..2].copy_from_slice(&[0x82, 32]);
                    sender.get_mut().write_all(&frame).await.unwrap();
                } else {
                    sender.send(warmup.clone()).await.unwrap();
                }
            }
            warmup_done.await.unwrap();
            let mut completed_ns = Vec::with_capacity(count);
            ready.wait().await;
            let start = Instant::now() + Duration::from_millis(50);
            tokio::time::sleep_until(start.into()).await;
            for sequence in 0..count {
                let due = if rate == 0 {
                    Instant::now()
                } else {
                    let offset =
                        (sequence as u128).checked_mul(1_000_000_000).unwrap() / rate as u128;
                    let due = start + Duration::from_nanos(u64::try_from(offset).unwrap());
                    // Avoid adding a timer await to messages whose deadline already passed.
                    if due > Instant::now() {
                        tokio::time::sleep_until(due.into()).await;
                    }
                    due
                };
                let mut payload = BytesMut::zeroed(32);
                payload[..8].copy_from_slice(&(sequence as u64).to_le_bytes());
                let due_ns = u64::try_from(due.duration_since(epoch).as_nanos()).unwrap();
                payload[16..24].copy_from_slice(&due_ns.to_le_bytes());
                let sent_ns = u64::try_from(epoch.elapsed().as_nanos()).unwrap();
                payload[8..16].copy_from_slice(&sent_ns.to_le_bytes());
                if RAW {
                    let mut frame = [0; 34];
                    frame[..2].copy_from_slice(&[0x82, 32]);
                    frame[2..].copy_from_slice(&payload);
                    sender.get_mut().write_all(&frame).await.unwrap();
                } else {
                    sender
                        .send(Message::Binary(payload.freeze()))
                        .await
                        .unwrap();
                }
                completed_ns.push(u64::try_from(epoch.elapsed().as_nanos()).unwrap());
            }
            let ((received, peer), task_polls) = peer_task.await.unwrap();
            (id, completed_ns, received, peer, task_polls)
        }));
    }
    // Analyze only after every connection finishes so reporting cannot delay a peer.
    let mut results = Vec::new();
    for task in tasks {
        results.push(task.await.unwrap());
    }
    for (id, completed, received, peer, task_polls) in results {
        assert_eq!(completed.len(), count);
        let latency: Vec<_> = received
            .received_ns
            .iter()
            .zip(&received.sent_ns)
            .map(|(received, sent)| received.checked_sub(*sent).unwrap())
            .collect();
        let send: Vec<_> = completed
            .iter()
            .zip(&received.sent_ns)
            .map(|(completed, sent)| completed.checked_sub(*sent).unwrap())
            .collect();
        let late: Vec<_> = received
            .sent_ns
            .iter()
            .zip(&received.due_ns)
            .map(|(sent, due)| sent.checked_sub(*due).unwrap())
            .collect();
        let scheduled: Vec<_> = received
            .received_ns
            .iter()
            .zip(&received.due_ns)
            .map(|(received, due)| received.checked_sub(*due).unwrap())
            .collect();
        // Saturated sends have no independent schedule; do not report setup cost as lateness.
        let [late_p99, scheduled_p99, scheduled_spread, late_spread]: [String; 4] = if rate == 0 {
            Default::default()
        } else {
            [
                percentile(&late, 99),
                percentile(&scheduled, 99),
                percentile(&scheduled, 99) - percentile(&scheduled, 1),
                percentile(&late, 99) - percentile(&late, 1),
            ]
            .map(|value| value.to_string())
        };
        let mut slowest: Vec<_> = (0..count).collect();
        slowest.sort_unstable_by_key(|&i| std::cmp::Reverse(latency[i]));
        let max_gap = received
            .received_ns
            .windows(2)
            .enumerate()
            .map(|(i, pair)| (pair[1] - pair[0], i + 1))
            .max()
            .unwrap();
        if TRACE {
            let reads = peer.read_polls();
            assert_eq!(
                reads.iter().map(|p| p.bytes).sum::<usize>(),
                (count + WARMUP_MESSAGES) * 34
            );
            let max_wake = task_polls
                .iter()
                .filter(|p| p.wake_ns != 0)
                .map(|p| p.start_ns.checked_sub(p.wake_ns).unwrap())
                .max()
                .unwrap_or(0);
            let max_poll = task_polls
                .iter()
                .map(|p| p.end_ns - p.start_ns)
                .max()
                .unwrap();
            println!(
                "trace,{id},{},{},{max_wake},{max_poll}",
                reads.len(),
                task_polls.len()
            );
            // Retain the polls around the worst delivery gap, plus long scheduler/I/O events.
            let gap_before = received.received_ns[max_gap.1 - 1];
            let gap_after = received.received_ns[max_gap.1];
            let mut previous_end = 0;
            for p in &task_polls {
                if (previous_end <= gap_after && p.end_ns >= gap_before)
                    || (p.wake_ns != 0 && p.start_ns - p.wake_ns >= TRACE_EVENT_NS)
                    || p.end_ns - p.start_ns >= TRACE_EVENT_NS
                {
                    println!("task,{id},{},{},{}", p.wake_ns, p.start_ns, p.end_ns);
                }
                previous_end = p.end_ns;
            }
            let mut previous_end = 0;
            for p in reads {
                if (previous_end <= gap_after && p.end_ns >= gap_before)
                    || p.start_ns - previous_end >= TRACE_EVENT_NS
                    || p.end_ns - p.start_ns >= TRACE_EVENT_NS
                {
                    println!(
                        "read,{id},{},{},{},{}",
                        p.start_ns, p.end_ns, p.bytes, p.pending
                    );
                }
                previous_end = p.end_ns;
            }
        }
        println!(
            "connection,{id},{workers},{count},{rate},{},{},{late_p99},{},{},{},{},{},{scheduled_p99},{scheduled_spread},{late_spread}",
            percentile(&latency, 99),
            percentile(&send, 99),
            received.sent_ns[0],
            received.received_ns[count - 1],
            max_gap.0,
            received.received_ns[max_gap.1 - 1],
            received.received_ns[max_gap.1],
        );
        for &i in slowest.iter().take(20) {
            println!(
                "slow,{id},{i},{},{},{},{},{}",
                received.due_ns[i],
                received.sent_ns[i],
                completed[i],
                received.received_ns[i],
                latency[i]
            );
        }
    }
}

fn check_wake_forwarding() {
    struct Counter(std::sync::atomic::AtomicUsize);
    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    let counter = Arc::new(Counter(std::sync::atomic::AtomicUsize::new(0)));
    let trace = Arc::new(WakeTrace {
        parent: Waker::from(counter.clone()),
        epoch: Instant::now(),
        first_wake_ns: AtomicU64::new(0),
    });
    let waker = Waker::from(trace.clone());
    waker.wake_by_ref();
    let first = trace.first_wake_ns.load(Ordering::Relaxed);
    waker.wake();
    assert_eq!(counter.0.load(Ordering::Relaxed), 2);
    assert!(first > 0);
    assert_eq!(trace.first_wake_ns.load(Ordering::Relaxed), first);
}

fn main() {
    // Cargo passes --bench to harness-free benchmark executables.
    let args: Vec<_> = std::env::args()
        .skip(1)
        .filter(|arg| arg != "--bench")
        .collect();
    let values: Vec<usize> = if args == ["--test"] {
        check_wake_forwarding();
        vec![1, 0, 1, 16, 1_000]
    } else if args.is_empty() {
        vec![1, 0, 1, 128, 1_000]
    } else {
        assert!(
            args.len() == 5 || args.len() == 7,
            "workers receiver_workers connections count rate_per_connection [ws|raw off|on]"
        );
        args[..5].iter().map(|arg| arg.parse().unwrap()).collect()
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(values[0])
        .enable_all()
        .build()
        .unwrap();
    let receiver = (values[1] > 0).then(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(values[1])
            .enable_all()
            .build()
            .unwrap()
    });
    let handle = receiver.as_ref().unwrap_or(&runtime).handle().clone();
    let (raw, trace) = if args.len() == 7 {
        assert!(matches!(args[5].as_str(), "ws" | "raw"));
        assert!(matches!(args[6].as_str(), "off" | "on"));
        (args[5] == "raw", args[6] == "on")
    } else {
        (false, false)
    };
    match (raw, trace) {
        (false, false) => runtime.block_on(run::<false, false>(
            values[0], handle, values[2], values[3], values[4],
        )),
        (false, true) => runtime.block_on(run::<false, true>(
            values[0], handle, values[2], values[3], values[4],
        )),
        (true, false) => runtime.block_on(run::<true, false>(
            values[0], handle, values[2], values[3], values[4],
        )),
        (true, true) => runtime.block_on(run::<true, true>(
            values[0], handle, values[2], values[3], values[4],
        )),
    }
}
