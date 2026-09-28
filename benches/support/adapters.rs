//! Adapter-only round trips, including their drivers but excluding WebSocket framing.
use criterion::{BenchmarkId, Criterion, Throughput};
use std::{
    hint::black_box,
    time::{Duration, Instant},
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

async fn exchange<C, P>(mut client: C, mut server: P, size: usize, iterations: u64) -> Duration
where
    C: AsyncRead + AsyncWrite + Unpin,
    P: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let peer = tokio::spawn(async move {
        let mut data = vec![0; size];
        for _ in 0..iterations + 2 {
            server.read_exact(&mut data).await.unwrap();
            server.write_all(&data).await.unwrap();
            server.flush().await.unwrap();
        }
    });
    let payload = vec![0x42; size];
    let mut received = vec![0; size];
    client.write_all(&payload).await.unwrap();
    client.flush().await.unwrap();
    client.read_exact(&mut received).await.unwrap();
    assert_eq!(received, payload);
    let start = Instant::now();
    for _ in 0..iterations {
        client.write_all(black_box(&payload)).await.unwrap();
        client.flush().await.unwrap();
        client.read_exact(&mut received).await.unwrap();
        black_box(&received);
    }
    let elapsed = start.elapsed();
    client.write_all(&payload).await.unwrap();
    client.flush().await.unwrap();
    client.read_exact(&mut received).await.unwrap();
    assert_eq!(received, payload);
    peer.await.unwrap();
    elapsed
}

#[cfg(feature = "http2")]
pub fn h2(c: &mut Criterion) {
    use sockudo_ws::http2::stream::Http2Stream;
    let mut group = c.benchmark_group("transport/extended/h2_adapter");
    group.throughput(Throughput::Elements(1));
    for window in [1024, 65535] {
        for size in [32, 65536] {
            group.bench_function(BenchmarkId::new(format!("window{window}"), size), |b| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                b.iter_custom(|iterations| {
                    runtime.block_on(async {
                        let (client, server) = tokio::io::duplex(65536);
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        let server_driver = tokio::spawn(async move {
                            let mut connection = h2::server::Builder::new()
                                .initial_window_size(window)
                                .handshake(server)
                                .await
                                .unwrap();
                            let (request, mut response) =
                                connection.accept().await.unwrap().unwrap();
                            let send = response
                                .send_response(http::Response::new(()), false)
                                .unwrap();
                            tx.send(Http2Stream::new(send, request.into_body()))
                                .ok()
                                .unwrap();
                            while connection.accept().await.is_some() {}
                        });
                        let (mut request, connection) = h2::client::Builder::new()
                            .initial_window_size(window)
                            .handshake(client)
                            .await
                            .unwrap();
                        let client_driver = tokio::spawn(async move {
                            connection.await.unwrap();
                        });
                        let (response, send) = request
                            .send_request(
                                http::Request::builder()
                                    .uri("https://localhost/adapter")
                                    .body(())
                                    .unwrap(),
                                false,
                            )
                            .unwrap();
                        let client = Http2Stream::new(send, response.await.unwrap().into_body());
                        let elapsed = exchange(client, rx.await.unwrap(), size, iterations).await;
                        // Connection drivers intentionally outlive their DATA streams; join their cancellation outside timing.
                        server_driver.abort();
                        client_driver.abort();
                        let _ = server_driver.await;
                        let _ = client_driver.await;
                        elapsed
                    })
                });
            });
        }
    }
    group.finish();
}

#[cfg(feature = "http3")]
#[path = "../../tests/support/h3_pair.rs"]
pub mod h3_pair;

#[cfg(feature = "http3")]
pub fn h3(c: &mut Criterion) {
    use sockudo_ws::http3::stream::{Http3ClientStream, Http3ServerStream, Http3Stream};
    let mut group = c.benchmark_group("transport/extended/h3_adapter");
    group.throughput(Throughput::Elements(1));
    for size in [32, 65536] {
        group.bench_function(BenchmarkId::new("data", size), |b| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            b.iter_custom(|iterations| {
                runtime.block_on(async {
                    let (client, server, connection) = h3_pair::pair().await;
                    let elapsed = exchange(
                        Http3ClientStream::new(client),
                        Http3ServerStream::new(server),
                        size,
                        iterations,
                    )
                    .await;
                    drop(connection);
                    elapsed
                })
            });
        });
        group.bench_function(BenchmarkId::new("raw_quic", size), |b| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            b.iter_custom(|iterations| {
                runtime.block_on(async {
                    let (endpoints, [client, server]) = h3_pair::quic_pair().await;
                    let (send, recv) = client.open_bi().await.unwrap();
                    // QUIC only advertises the stream after its first write, so accept concurrently.
                    let accept_connection = server.clone();
                    let peer = tokio::spawn(async move {
                        let (send, recv) = accept_connection.accept_bi().await.unwrap();
                        let mut peer = Http3Stream::new(send, recv);
                        let mut data = vec![0; size];
                        for _ in 0..iterations + 2 {
                            peer.read_exact(&mut data).await.unwrap();
                            peer.write_all(&data).await.unwrap();
                            peer.flush().await.unwrap();
                        }
                    });
                    let mut io = Http3Stream::new(send, recv);
                    let payload = vec![0x42; size];
                    let mut received = vec![0; size];
                    io.write_all(&payload).await.unwrap();
                    io.flush().await.unwrap();
                    io.read_exact(&mut received).await.unwrap();
                    assert_eq!(received, payload);
                    let start = Instant::now();
                    for _ in 0..iterations {
                        io.write_all(black_box(&payload)).await.unwrap();
                        io.flush().await.unwrap();
                        io.read_exact(&mut received).await.unwrap();
                        black_box(&received);
                    }
                    let elapsed = start.elapsed();
                    io.write_all(&payload).await.unwrap();
                    io.flush().await.unwrap();
                    io.read_exact(&mut received).await.unwrap();
                    assert_eq!(received, payload);
                    peer.await.unwrap();
                    drop((client, server));
                    for endpoint in endpoints {
                        endpoint.close(0u32.into(), b"benchmark complete");
                    }
                    elapsed
                })
            });
        });
    }
    group.finish();
}
