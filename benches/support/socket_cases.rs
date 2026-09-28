//! Real socket round trips. Setup and two payload checks are outside timing.
use criterion::{BenchmarkId, Criterion, Throughput};
#[cfg(feature = "tokio-runtime")]
use futures_util::{SinkExt, StreamExt};
use sockudo_ws::{Config, Message};
use std::{hint::black_box, time::Instant};

fn config(window: bool) -> Config {
    let builder = Config::builder().auto_ping(false).idle_timeout(0);
    if window {
        #[cfg(feature = "http2")]
        let builder = builder
            .http2_stream_window_size(1024)
            .http2_connection_window_size(4096);
        #[cfg(feature = "http3")]
        let builder = builder.http3_stream_window_size(1024);
        builder.build()
    } else {
        builder.build()
    }
}
macro_rules! exchange {
    ($ws:ident,$message:ident,$iterations:ident) => {{
        $ws.send($message.clone()).await.unwrap();
        assert_eq!(
            $ws.next().await.unwrap().unwrap().as_bytes(),
            $message.as_bytes()
        );
        let start = Instant::now();
        for _ in 0..$iterations {
            $ws.send(black_box($message.clone())).await.unwrap();
            black_box($ws.next().await.unwrap().unwrap());
        }
        let elapsed = start.elapsed();
        $ws.send($message.clone()).await.unwrap();
        assert_eq!(
            $ws.next().await.unwrap().unwrap().as_bytes(),
            $message.as_bytes()
        );
        elapsed
    }};
}

#[cfg(feature = "tokio-runtime")]
pub fn tokio_tcp(c: &mut Criterion) {
    let mut group = c.benchmark_group("transport/extended/tokio_tcp_roundtrip");
    group.throughput(Throughput::Elements(1));
    for size in [32, 8192, 131073] {
        {
            group.bench_function(BenchmarkId::new("default", size), |b| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let message = Message::binary(vec![0x42; size]);
                b.iter_custom(|iterations| {
                    runtime.block_on(async {
                        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                        let address = listener.local_addr().unwrap();
                        let peer = tokio::spawn(async move {
                            let (socket, _) = listener.accept().await.unwrap();
                            socket.set_nodelay(true).unwrap();
                            let mut ws = sockudo_ws::WebSocketStream::server(socket, config(false));
                            for _ in 0..iterations + 2 {
                                let message = ws.next().await.unwrap().unwrap();
                                ws.send(message).await.unwrap();
                            }
                        });
                        let socket = tokio::net::TcpStream::connect(address).await.unwrap();
                        socket.set_nodelay(true).unwrap();
                        let mut ws = sockudo_ws::WebSocketStream::client(socket, config(false));
                        let elapsed = exchange!(ws, message, iterations);
                        drop(ws);
                        peer.await.unwrap();
                        elapsed
                    })
                });
            });
        }
    }
    group.finish();
}

#[cfg(all(feature = "tokio-runtime", feature = "http2"))]
pub fn tokio_http2(c: &mut Criterion) {
    let mut group = c.benchmark_group("transport/extended/tokio_http2_roundtrip");
    group.throughput(Throughput::Elements(1));
    for size in [32, 8192, 131073] {
        for window in [false, true] {
            group.bench_function(
                BenchmarkId::new(if window { "window1024" } else { "default" }, size),
                |b| {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    let message = Message::binary(vec![0x42; size]);
                    b.iter_custom(|iterations| {
                        runtime.block_on(async {
                            let listener =
                                tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                            let address = listener.local_addr().unwrap();
                            let peer = tokio::spawn(async move {
                                let (socket, _) = listener.accept().await.unwrap();
                                socket.set_nodelay(true).unwrap();
                                sockudo_ws::WebSocketServer::<sockudo_ws::Http2>::new(config(
                                    window,
                                ))
                                .serve(socket, move |mut ws, _| async move {
                                    for _ in 0..iterations + 2 {
                                        let message = ws.next().await.unwrap().unwrap();
                                        ws.send(message).await.unwrap();
                                    }
                                    // Preserve queued DATA when the handler finishes.
                                    ws.close(1000, "").await.unwrap();
                                })
                                .await
                                .unwrap();
                            });
                            let socket = tokio::net::TcpStream::connect(address).await.unwrap();
                            socket.set_nodelay(true).unwrap();
                            let mut ws = sockudo_ws::WebSocketClient::<sockudo_ws::Http2>::new(
                                config(window),
                            )
                            .connect(socket, "https://localhost/bench", None)
                            .await
                            .unwrap();
                            let elapsed = exchange!(ws, message, iterations);
                            assert!(matches!(ws.next().await, Some(Ok(Message::Close(_)))));
                            drop(ws);
                            peer.await.unwrap();
                            elapsed
                        })
                    });
                },
            );
        }
    }
    group.finish();
}

#[cfg(all(feature = "tokio-runtime", feature = "http3"))]
pub fn tokio_http3(c: &mut Criterion) {
    let mut group = c.benchmark_group("transport/extended/tokio_http3_roundtrip");
    group.throughput(Throughput::Elements(1));
    for size in [32, 8192, 131073] {
        for window in [false, true] {
            group.bench_function(
                BenchmarkId::new(
                    if window {
                        "client_window1024"
                    } else {
                        "default"
                    },
                    size,
                ),
                |b| {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    let message = Message::binary(vec![0x42; size]);
                    let (server_tls, client_tls) = crate::tls_fixture::configs(true);
                    b.iter_custom(|iterations| {
                        runtime.block_on(async {
                            let endpoint = quinn::Endpoint::server(
                                quinn::ServerConfig::with_crypto(std::sync::Arc::new(
                                    quinn::crypto::rustls::QuicServerConfig::try_from(
                                        server_tls.clone(),
                                    )
                                    .unwrap(),
                                )),
                                "127.0.0.1:0".parse().unwrap(),
                            )
                            .unwrap();
                            let server =
                                sockudo_ws::WebSocketServer::<sockudo_ws::Http3>::from_endpoint(
                                    endpoint.clone(),
                                    config(window),
                                );
                            let address = endpoint.local_addr().unwrap();
                            let peer = tokio::spawn(async move {
                                server
                                    .serve(move |mut ws, _| async move {
                                        for _ in 0..iterations + 2 {
                                            let message = ws.next().await.unwrap().unwrap();
                                            ws.send(message).await.unwrap();
                                        }
                                    })
                                    .await
                                    .unwrap();
                            });
                            let mut ws = sockudo_ws::WebSocketClient::<sockudo_ws::Http3>::new(
                                config(window),
                            )
                            .connect(address, "localhost", "/bench", client_tls.clone())
                            .await
                            .unwrap();
                            let elapsed = exchange!(ws, message, iterations);
                            drop(ws);
                            endpoint.close(0u32.into(), b"benchmark complete");
                            peer.await.unwrap();
                            elapsed
                        })
                    });
                },
            );
        }
    }
    group.finish();
}

#[cfg(feature = "compio-runtime")]
pub fn compio_tcp(c: &mut Criterion) {
    let mut group = c.benchmark_group("transport/extended/compio_tcp_roundtrip");
    group.throughput(Throughput::Elements(1));
    for size in [32, 8192, 131073] {
        {
            group.bench_function(BenchmarkId::new("default", size), |b| {
                let runtime = compio::runtime::Runtime::new().unwrap();
                let message = Message::binary(vec![0x42; size]);
                b.iter_custom(|iterations| {
                    runtime.block_on(async {
                        let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                        let address = listener.local_addr().unwrap();
                        let peer = compio::runtime::spawn(async move {
                            let (socket, _) = listener.accept().await.unwrap();
                            socket.set_nodelay(true).unwrap();
                            let mut ws =
                                sockudo_ws::CompioWebSocketStream::server(socket, config(false));
                            for _ in 0..iterations + 2 {
                                let message = ws.next().await.unwrap().unwrap();
                                ws.send(message).await.unwrap();
                            }
                        });
                        let socket = compio::net::TcpStream::connect(address).await.unwrap();
                        socket.set_nodelay(true).unwrap();
                        let mut ws =
                            sockudo_ws::CompioWebSocketStream::client(socket, config(false));
                        let elapsed = exchange!(ws, message, iterations);
                        drop(ws);
                        peer.await.unwrap();
                        elapsed
                    })
                });
            });
        }
    }
    group.finish();
}

#[cfg(all(feature = "compio-runtime", feature = "http2"))]
pub fn compio_http2(c: &mut Criterion) {
    let mut group = c.benchmark_group("transport/extended/compio_http2_roundtrip");
    group.throughput(Throughput::Elements(1));
    for size in [32, 8192, 131073] {
        for window in [false, true] {
            group.bench_function(
                BenchmarkId::new(if window { "window1024" } else { "default" }, size),
                |b| {
                    let runtime = compio::runtime::Runtime::new().unwrap();
                    let message = Message::binary(vec![0x42; size]);
                    b.iter_custom(|iterations| {
                        runtime.block_on(async {
                            let listener =
                                compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                            let address = listener.local_addr().unwrap();
                            let peer = compio::runtime::spawn(async move {
                                let (socket, _) = listener.accept().await.unwrap();
                                socket.set_nodelay(true).unwrap();
                                sockudo_ws::compio::serve_http2(
                                    socket,
                                    config(window),
                                    move |mut ws, _| async move {
                                        for _ in 0..iterations + 2 {
                                            let message = ws.next().await.unwrap().unwrap();
                                            ws.send(message).await.unwrap();
                                        }
                                        // Preserve queued DATA when the handler finishes.
                                        ws.close(1000, "").await.unwrap();
                                    },
                                )
                                .await
                                .unwrap();
                            });
                            let socket = compio::net::TcpStream::connect(address).await.unwrap();
                            socket.set_nodelay(true).unwrap();
                            let mut ws = sockudo_ws::compio::connect_http2(
                                socket,
                                "https://localhost/bench",
                                None,
                                config(window),
                            )
                            .await
                            .unwrap();
                            let elapsed = exchange!(ws, message, iterations);
                            assert!(matches!(ws.next().await, Some(Ok(Message::Close(_)))));
                            drop(ws);
                            peer.await.unwrap();
                            elapsed
                        })
                    });
                },
            );
        }
    }
    group.finish();
}

#[cfg(all(feature = "compio-runtime", feature = "http3"))]
pub fn compio_http3(c: &mut Criterion) {
    let mut group = c.benchmark_group("transport/extended/compio_http3_roundtrip");
    group.throughput(Throughput::Elements(1));
    for size in [32, 8192, 131073] {
        for window in [false, true] {
            group.bench_function(
                BenchmarkId::new(
                    if window {
                        "client_window1024"
                    } else {
                        "default"
                    },
                    size,
                ),
                |b| {
                    let runtime = compio::runtime::Runtime::new().unwrap();
                    let message = Message::binary(vec![0x42; size]);
                    let (server_tls, client_tls) = crate::tls_fixture::configs(true);
                    b.iter_custom(|iterations| {
                        runtime.block_on(async {
                            let endpoint =
                                compio::quic::ServerBuilder::new_with_rustls_server_config(
                                    server_tls.clone(),
                                )
                                .with_alpn_protocols(&["h3"])
                                .bind("127.0.0.1:0")
                                .await
                                .unwrap();
                            let server = sockudo_ws::compio::CompioHttp3Server::from_endpoint(
                                endpoint.clone(),
                                config(window),
                            );
                            let address = endpoint.local_addr().unwrap();
                            let peer = compio::runtime::spawn(async move {
                                server
                                    .serve(move |mut ws, _| async move {
                                        for _ in 0..iterations + 2 {
                                            let message = ws.next().await.unwrap().unwrap();
                                            ws.send(message).await.unwrap();
                                        }
                                    })
                                    .await
                                    .unwrap();
                            });
                            let mut ws = sockudo_ws::compio::connect_http3(
                                address,
                                "localhost",
                                "/bench",
                                None,
                                client_tls.clone(),
                                config(window),
                            )
                            .await
                            .unwrap();
                            let elapsed = exchange!(ws, message, iterations);
                            drop(ws);
                            endpoint.close(0u32.into(), b"benchmark complete");
                            peer.await.unwrap();
                            elapsed
                        })
                    });
                },
            );
        }
    }
    group.finish();
}
