//! Closed-loop, already-arrived bursts over TCP loopback; never waits to fill a batch.
//! Client sender and server peer use separate current-thread runtimes on distinct physical cores.
//! --case mode bytes burst groups sender_cpu peer_cpu idle_us nodelay
//! CSV preserves readiness, call, completion and peer-decode timestamps per message.
//! The burst ACK and idle interval are outside the measured delivery interval.

use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use sockudo_ws::{Config, Message, WebSocketStream};
use tokio::net::TcpStream;

fn pin_thread(cpu: &str) {
    if cpu == "-" {
        return;
    }
    let thread = std::fs::read_link("/proc/thread-self").unwrap();
    let tid = thread.file_name().unwrap().to_str().unwrap();
    let result = std::process::Command::new("taskset")
        .args(["-pc", cpu, tid])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    eprintln!("{}", String::from_utf8_lossy(&result.stdout).trim());
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn stamp(epoch: Instant) -> u64 {
    u64::try_from(epoch.elapsed().as_nanos()).unwrap()
}

fn main() {
    let mut args: Vec<_> = std::env::args()
        .skip(1)
        .filter(|x| x != "--bench")
        .collect();
    if args.is_empty() {
        args.extend(["--case", "send", "64", "1", "8", "-", "-", "0", "true"].map(str::to_owned));
    }
    assert_eq!(
        args.len(),
        9,
        "--case mode bytes burst groups sender_cpu peer_cpu idle_us nodelay"
    );
    assert_eq!(args[0], "--case");
    let mode = args[1].clone();
    assert!(matches!(
        mode.as_str(),
        "send" | "batch" | "split" | "futures"
    ));
    let size: usize = args[2].parse().unwrap();
    let burst: usize = args[3].parse().unwrap();
    let groups: usize = args[4].parse().unwrap();
    let idle_us: u64 = args[7].parse().unwrap();
    let nodelay: bool = args[8].parse().unwrap();
    assert!(size >= 8 && burst > 0 && groups > 0);
    let count = burst.checked_mul(groups).unwrap();
    let warmup = burst.checked_mul(16).unwrap();
    let total = count.checked_add(warmup).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let sender = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (peer, _) = listener.accept().unwrap();
    for stream in [&sender, &peer] {
        stream.set_nodelay(nodelay).unwrap();
        assert_eq!(stream.nodelay().unwrap(), nodelay);
        stream.set_nonblocking(true).unwrap();
    }
    let epoch = Instant::now();
    let (ack_tx, mut ack_rx) = tokio::sync::mpsc::channel(1);
    let peer_cpu = args[6].clone();
    let receiver = std::thread::spawn(move || {
        pin_thread(&peer_cpu);
        runtime().block_on(async {
            let socket = TcpStream::from_std(peer).unwrap();
            let mut ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
                socket,
                tokio_tungstenite::tungstenite::protocol::Role::Server,
                None,
            )
            .await;
            let mut received = Vec::with_capacity(count);
            for sequence in 0..total {
                let message = ws.next().await.unwrap().unwrap();
                let at = stamp(epoch);
                let payload = message.into_data();
                assert_eq!(payload.len(), size);
                assert_eq!(
                    u64::from_le_bytes(payload[..8].try_into().unwrap()),
                    sequence as u64
                );
                assert!(payload[8..].iter().all(|&byte| byte == 0x5a));
                if sequence >= warmup {
                    received.push(at);
                }
                if (sequence + 1).is_multiple_of(burst) {
                    ack_tx.send(()).await.unwrap();
                }
            }
            received
        })
    });
    pin_thread(&args[5]);
    let samples = runtime().block_on(async {
        let socket = TcpStream::from_std(sender).unwrap();
        // Keep the hard encoded-byte limit out of this flush-policy comparison.
        let capacity = size.checked_add(14).unwrap().checked_mul(burst).unwrap();
        let config = Config::builder().max_backpressure(capacity).build();
        let ws = WebSocketStream::client(socket, config);
        let (mut unified, mut split, mut futures, _reader, _futures_reader) = match mode.as_str() {
            "split" => {
                let (reader, writer) = ws.split();
                (None, Some(writer), None, Some(reader), None)
            }
            "futures" => {
                let (writer, reader) = StreamExt::split(ws);
                (None, None, Some(writer), None, Some(reader))
            }
            _ => (Some(ws), None, None, None, None),
        };
        let mut samples = Vec::with_capacity(count);
        for group in 0..groups + 16 {
            // Every message in this burst is available before the shared ready timestamp.
            let messages: Vec<_> = (0..burst)
                .map(|index| {
                    let mut data = vec![0x5a; size];
                    let sequence = group * burst + index;
                    data[..8].copy_from_slice(&(sequence as u64).to_le_bytes());
                    Message::Binary(Bytes::from(data))
                })
                .collect();
            if idle_us != 0 {
                tokio::time::sleep(Duration::from_micros(idle_us)).await;
            }
            let mut group_samples = Vec::with_capacity(burst);
            let ready = stamp(epoch);
            for message in messages {
                let begin = stamp(epoch);
                match mode.as_str() {
                    "send" => unified.as_mut().unwrap().send(message).await.unwrap(),
                    "batch" => unified.as_mut().unwrap().feed(message).await.unwrap(),
                    "split" => split.as_mut().unwrap().send(message).await.unwrap(),
                    "futures" => futures.as_mut().unwrap().send(message).await.unwrap(),
                    _ => unreachable!(),
                }
                group_samples.push((ready, begin, stamp(epoch)));
            }
            if mode == "batch" {
                unified.as_mut().unwrap().flush().await.unwrap();
                let completed = stamp(epoch);
                // feed completion is not send completion: report the shared flush boundary.
                for sample in &mut group_samples {
                    sample.2 = completed;
                }
            }
            ack_rx.recv().await.unwrap();
            if group >= 16 {
                samples.extend(group_samples);
            }
        }
        samples
    });
    let received = receiver.join().unwrap();
    assert_eq!(samples.len(), count);
    assert_eq!(received.len(), count);
    println!("mode,bytes,burst,group,index,ready_ns,begin_ns,completed_ns,received_ns");
    for (sequence, ((ready, begin, completed), received)) in
        samples.into_iter().zip(received).enumerate()
    {
        assert!(received >= ready && completed >= begin);
        println!(
            "{mode},{size},{burst},{},{},{ready},{begin},{completed},{received}",
            sequence / burst,
            sequence % burst
        );
    }
}
