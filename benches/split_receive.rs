//! Valid receive work only: parser segmentation and steady-state split TCP receive.
//! Socket/runtime construction and split are outside timing. TCP cases include
//! peer writes, loopback, runtime I/O, parsing, delivery and message destruction.

use bytes::BytesMut;
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use sockudo_ws::frame::{FrameParser, encode_frame};
use sockudo_ws::{Config, DeflateConfig, OpCode};
use std::hint::black_box;
use std::time::Instant;

fn config() -> Config {
    Config::builder().auto_ping(false).idle_timeout(0).build()
}

fn parser_cases(c: &mut Criterion) {
    for masked in [false, true] {
        for size in [64, 126, 4096] {
            for partial in [false, true] {
                let payload = vec![b'x'; size];
                let mut wire = BytesMut::new();
                encode_frame(
                    &mut wire,
                    OpCode::Binary,
                    &payload,
                    true,
                    masked.then_some([7, 13, 19, 23]),
                );
                let mut parser = FrameParser::new(1 << 20, masked);
                let cut = if partial { wire.len() - 1 } else { wire.len() };
                let mut parse = |mut input: BytesMut| {
                    let tail = input.split_off(cut);
                    let frame = if partial {
                        assert!(parser.parse(&mut input).unwrap().is_none());
                        input.extend_from_slice(&tail);
                        parser.parse(&mut input).unwrap().unwrap()
                    } else {
                        parser.parse(&mut input).unwrap().unwrap()
                    };
                    black_box(frame)
                };
                assert_eq!(parse(wire.clone()).payload.as_ref(), payload);
                let name = format!("split_receive/parser_masked{masked}_partial{partial}_{size}");
                c.bench_function(&name, |b| {
                    b.iter_batched(|| wire.clone(), &mut parse, BatchSize::SmallInput)
                });
                assert_eq!(parse(wire.clone()).payload.as_ref(), payload);
            }
        }
    }
}

// Keep concrete reader types in each timed loop; the constructor is the only
// difference between plain and compression-capable (RSV1-clear) cases.
macro_rules! tokio_case {
    ($c:expr, $size:expr, $name:expr, $construct:expr) => {{
        $c.bench_function($name, |b| {
            use tokio::io::AsyncWriteExt;
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let payload = vec![b'x'; $size];
            let mut wire = BytesMut::new();
            encode_frame(&mut wire, OpCode::Binary, &payload, true, None);
            let (mut reader, writer, mut peer) = rt.block_on(async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let stream = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
                    .await
                    .unwrap();
                let (peer, _) = listener.accept().await.unwrap();
                peer.set_nodelay(true).unwrap();
                let (reader, writer) = ($construct)(stream).split();
                (reader, writer, peer)
            });
            rt.block_on(async {
                peer.write_all(&wire).await.unwrap();
                assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), payload);
            });
            b.iter_custom(|iters| {
                rt.block_on(async {
                    let start = Instant::now();
                    for _ in 0..iters {
                        peer.write_all(black_box(&wire)).await.unwrap();
                        black_box(reader.next().await.unwrap().unwrap());
                    }
                    start.elapsed()
                })
            });
            rt.block_on(async {
                peer.write_all(&wire).await.unwrap();
                assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), payload);
                drop((reader, writer, peer));
                tokio::task::yield_now().await;
            });
        });
    }};
}

macro_rules! compio_case {
    ($c:expr, $size:expr, $name:expr, $construct:expr) => {{
        $c.bench_function($name, |b| {
            use compio::io::AsyncWriteExt;
            let rt = compio::runtime::Runtime::new().unwrap();
            let payload = vec![b'x'; $size];
            let mut wire = BytesMut::new();
            encode_frame(&mut wire, OpCode::Binary, &payload, true, None);
            let wire = wire.freeze();
            let (mut reader, writer, mut peer) = rt.block_on(async {
                let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let stream = compio::net::TcpStream::connect(listener.local_addr().unwrap())
                    .await
                    .unwrap();
                let (peer, _) = listener.accept().await.unwrap();
                peer.set_nodelay(true).unwrap();
                let (reader, writer) = ($construct)(stream).split();
                (reader, writer, peer)
            });
            rt.block_on(async {
                peer.write_all(wire.clone()).await.0.unwrap();
                assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), payload);
            });
            b.iter_custom(|iters| {
                rt.block_on(async {
                    let start = Instant::now();
                    for _ in 0..iters {
                        peer.write_all(black_box(wire.clone())).await.0.unwrap();
                        black_box(reader.next().await.unwrap().unwrap());
                    }
                    start.elapsed()
                })
            });
            rt.block_on(async {
                peer.write_all(wire.clone()).await.0.unwrap();
                assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), payload);
                drop((reader, writer, peer));
                compio::runtime::spawn(async {}).await.unwrap();
            });
        });
    }};
}

fn tcp_cases(c: &mut Criterion) {
    for size in [64, 4096] {
        tokio_case!(
            c,
            size,
            &format!("split_receive/tokio_plain_{size}"),
            |io| sockudo_ws::WebSocketStream::client(io, config())
        );
        tokio_case!(
            c,
            size,
            &format!("split_receive/tokio_compressed_{size}"),
            |io| sockudo_ws::CompressedWebSocketStream::client(
                io,
                config(),
                DeflateConfig::default()
            )
        );
        compio_case!(
            c,
            size,
            &format!("split_receive/compio_plain_{size}"),
            |io| sockudo_ws::CompioWebSocketStream::client(io, config())
        );
        compio_case!(
            c,
            size,
            &format!("split_receive/compio_compressed_{size}"),
            |io| sockudo_ws::compio::CompioCompressedWebSocketStream::client(
                io,
                config(),
                DeflateConfig::default()
            )
        );
    }
}

criterion_group!(benches, parser_cases, tcp_cases);
criterion_main!(benches);
