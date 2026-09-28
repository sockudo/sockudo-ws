//! HTTP syntax and connection lifecycle costs; no peer scheduling in syntax cases.
use criterion::{Criterion, Throughput};
use sockudo_ws::handshake::{
    build_request, build_response, generate_accept_key, parse_request, parse_response,
};
use std::hint::black_box;

pub fn syntax(c: &mut Criterion) {
    let key = "dGhlIHNhbXBsZSBub25jZQ==";
    let accept = generate_accept_key(key);
    let mut group = c.benchmark_group("lifecycle/core/http1");
    group.throughput(Throughput::Elements(1));
    for extensions in [false, true] {
        let extension = extensions.then_some("permessage-deflate; client_max_window_bits");
        let protocol = extensions.then_some("events");
        let request = build_request("localhost", "/events", key, protocol, extension);
        let response = build_response(&accept, protocol, extension);
        assert_eq!(parse_request(&request).unwrap().unwrap().1, request.len());
        assert_eq!(
            parse_response(&response).unwrap().unwrap().1,
            response.len()
        );
        let name = if extensions { "extensions" } else { "plain" };
        group.bench_function(format!("parse_request_{name}"), |b| {
            b.iter(|| black_box(parse_request(black_box(&request)).unwrap()))
        });
        group.bench_function(format!("parse_response_{name}"), |b| {
            b.iter(|| black_box(parse_response(black_box(&response)).unwrap()))
        });
        group.bench_function(format!("build_request_{name}"), |b| {
            b.iter(|| {
                black_box(build_request(
                    black_box("localhost"),
                    black_box("/events"),
                    black_box(key),
                    protocol,
                    extension,
                ))
            })
        });
        group.bench_function(format!("build_response_{name}"), |b| {
            b.iter(|| black_box(build_response(black_box(&accept), protocol, extension)))
        });
    }
    group.finish();
}

#[cfg(feature = "tokio-runtime")]
pub fn tokio(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut group = c.benchmark_group("lifecycle/extended/tokio");
    group.bench_function("construct_drop", |b| {
        b.iter_custom(|iterations| {
            runtime.block_on(async {
                let mut elapsed = std::time::Duration::ZERO;
                for _ in 0..iterations {
                    let io = crate::controlled_io::Output::new(usize::MAX, false, false);
                    let config = sockudo_ws::Config::default();
                    let start = std::time::Instant::now();
                    // The controlled transport has no allocation or destructor work of its own.
                    black_box(sockudo_ws::WebSocketStream::client(io, config));
                    elapsed += start.elapsed();
                }
                elapsed
            })
        });
    });
    group.bench_function("split", |b| {
        b.iter_custom(|iterations| {
            runtime.block_on(async {
                let mut elapsed = std::time::Duration::ZERO;
                for _ in 0..iterations {
                    let (io, _peer) = tokio::io::duplex(4096);
                    let ws = sockudo_ws::WebSocketStream::client(io, sockudo_ws::Config::default());
                    let start = std::time::Instant::now();
                    let halves = ws.split();
                    elapsed += start.elapsed();
                    drop(halves);
                    tokio::task::yield_now().await;
                }
                elapsed
            })
        })
    });
    group.bench_function("http1_upgrade", |b| {
        b.iter_custom(|iterations| {
            runtime.block_on(async {
                let mut elapsed = std::time::Duration::ZERO;
                for _ in 0..iterations {
                    let (mut client, mut server) = tokio::io::duplex(4096);
                    let start = std::time::Instant::now();
                    let (client_result, server_result) = tokio::join!(
                        sockudo_ws::handshake::client_handshake(
                            &mut client,
                            "localhost",
                            "/events",
                            None
                        ),
                        sockudo_ws::handshake::server_handshake(&mut server),
                    );
                    elapsed += start.elapsed();
                    client_result.unwrap();
                    server_result.unwrap();
                }
                elapsed
            })
        })
    });
    group.finish();
}

#[cfg(feature = "compio-runtime")]
pub fn compio(c: &mut Criterion) {
    let mut group = c.benchmark_group("lifecycle/extended/compio");
    group.bench_function("construct_drop", |b| {
        let runtime = compio::runtime::Runtime::new().unwrap();
        b.iter_custom(|iterations| {
            runtime.block_on(async {
                let mut elapsed = std::time::Duration::ZERO;
                for _ in 0..iterations {
                    let io = crate::controlled_io::Output::new(usize::MAX, false, false);
                    let config = sockudo_ws::Config::default();
                    let start = std::time::Instant::now();
                    black_box(sockudo_ws::CompioWebSocketStream::client(io, config));
                    elapsed += start.elapsed();
                }
                elapsed
            })
        });
    });
    group.bench_function("split", |b| {
        let runtime = compio::runtime::Runtime::new().unwrap();
        b.iter_custom(|iterations| {
            runtime.block_on(async {
                let mut elapsed = std::time::Duration::ZERO;
                for _ in 0..iterations {
                    let io = crate::controlled_io::Output::new(usize::MAX, false, false);
                    let ws = sockudo_ws::CompioWebSocketStream::client(
                        io,
                        sockudo_ws::Config::default(),
                    );
                    let start = std::time::Instant::now();
                    let halves = ws.split();
                    elapsed += start.elapsed();
                    drop(halves);
                    compio::runtime::spawn(async {}).await.unwrap();
                }
                elapsed
            })
        });
    });
    group.finish();
}
