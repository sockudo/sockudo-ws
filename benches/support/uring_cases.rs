//! Linux io_uring poll-I/O bridge, including peer delivery and excluding setup.
use criterion::{BenchmarkId, Criterion, Throughput};
use futures_util::{SinkExt, StreamExt};
use sockudo_ws::{Config, Message, WebSocketStream, io_uring::UringStream};
use std::{hint::black_box, time::Instant};
pub fn benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("transport/extended/io_uring_roundtrip");
    group.throughput(Throughput::Elements(1));
    for size in [32, 65536] {
        group.bench_function(BenchmarkId::from_parameter(size), |b| {
            let message = Message::binary(vec![0x42; size]);
            b.iter_custom(|iterations| {
                tokio_uring::start(async {
                    let listener =
                        tokio_uring::net::TcpListener::bind("127.0.0.1:0".parse().unwrap())
                            .unwrap();
                    let address = listener.local_addr().unwrap();
                    let peer = tokio_uring::spawn(async move {
                        let (socket, _) = listener.accept().await.unwrap();
                        socket.set_nodelay(true).unwrap();
                        let mut ws = WebSocketStream::server(
                            UringStream::new(socket),
                            Config::builder().auto_ping(false).idle_timeout(0).build(),
                        );
                        for _ in 0..iterations + 2 {
                            let message = ws.next().await.unwrap().unwrap();
                            ws.send(message).await.unwrap();
                        }
                    });
                    let socket = tokio_uring::net::TcpStream::connect(address).await.unwrap();
                    socket.set_nodelay(true).unwrap();
                    let mut ws = WebSocketStream::client(
                        UringStream::new(socket),
                        Config::builder().auto_ping(false).idle_timeout(0).build(),
                    );
                    ws.send(message.clone()).await.unwrap();
                    assert_eq!(
                        ws.next().await.unwrap().unwrap().as_bytes(),
                        message.as_bytes()
                    );
                    let start = Instant::now();
                    for _ in 0..iterations {
                        ws.send(message.clone()).await.unwrap();
                        black_box(ws.next().await.unwrap().unwrap());
                    }
                    let elapsed = start.elapsed();
                    ws.send(message.clone()).await.unwrap();
                    assert_eq!(
                        ws.next().await.unwrap().unwrap().as_bytes(),
                        message.as_bytes()
                    );
                    drop(ws);
                    peer.await.unwrap();
                    elapsed
                })
            });
        });
    }
    group.finish();
}
