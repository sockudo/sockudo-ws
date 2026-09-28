//! Rustls handshake and steady-state WebSocket round trips are separate experiments.
use criterion::{BenchmarkId, Criterion, Throughput};
use futures_util::{SinkExt, StreamExt};
use sockudo_ws::{Config, Message, WebSocketStream};
use std::{hint::black_box, sync::Arc, time::Instant};

pub fn benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("transport/extended/rustls");
    group.throughput(Throughput::Elements(1));
    for size in [32, 65536] {
        group.bench_function(BenchmarkId::new("roundtrip", size), |b| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let (server, client) = crate::tls_fixture::configs(false);
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
            let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
            let message = Message::binary(vec![0x42; size]);
            b.iter_custom(|iterations| {
                runtime.block_on(async {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let address = listener.local_addr().unwrap();
                    let acceptor = acceptor.clone();
                    let peer = tokio::spawn(async move {
                        let (socket, _) = listener.accept().await.unwrap();
                        socket.set_nodelay(true).unwrap();
                        let socket = acceptor.accept(socket).await.unwrap();
                        let mut ws = WebSocketStream::server(
                            socket,
                            Config::builder().auto_ping(false).idle_timeout(0).build(),
                        );
                        for _ in 0..iterations + 2 {
                            let msg = ws.next().await.unwrap().unwrap();
                            ws.send(msg).await.unwrap();
                        }
                    });
                    let socket = tokio::net::TcpStream::connect(address).await.unwrap();
                    socket.set_nodelay(true).unwrap();
                    let socket = connector
                        .connect("localhost".try_into().unwrap(), socket)
                        .await
                        .unwrap();
                    let mut ws = WebSocketStream::client(
                        socket,
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
    group.bench_function("full_handshake", |b| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (server, mut client) = crate::tls_fixture::configs(false);
        client.resumption = rustls::client::Resumption::disabled();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
        b.iter_custom(|iterations| {
            runtime.block_on(async {
                let mut elapsed = std::time::Duration::ZERO;
                for _ in 0..iterations {
                    let (client, server) = tokio::io::duplex(65536);
                    let start = Instant::now();
                    let (client, server) = tokio::join!(
                        connector.connect("localhost".try_into().unwrap(), client),
                        acceptor.accept(server)
                    );
                    elapsed += start.elapsed();
                    black_box((client.unwrap(), server.unwrap()));
                }
                elapsed
            })
        });
    });
    group.finish();
}
