//! Comparative benchmarks: sockudo-ws vs tokio-websockets
//!
//! Run with: cargo bench --bench comparison_bench

use bytes::{Bytes, BytesMut};
use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};

// sockudo-ws imports
use sockudo_ws::frame::{FrameParser as SockudoParser, OpCode, encode_frame};
use sockudo_ws::simd::apply_mask;
use sockudo_ws::utf8::validate_utf8 as sockudo_validate_utf8;

/// Benchmark SIMD masking comparison
fn bench_masking_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("masking_comparison");

    for size in [64, 256, 1024, 4096, 16384, 65536] {
        group.throughput(Throughput::Bytes(size as u64));

        // sockudo-ws SIMD masking
        group.bench_with_input(BenchmarkId::new("sockudo_ws", size), &size, |b, &size| {
            let mut data = vec![0x42u8; size];
            let mask = [0x37, 0xfa, 0x21, 0x3d];

            b.iter(|| {
                apply_mask(black_box(&mut data), black_box(mask));
            });
        });

        // Standard XOR masking (reference implementation)
        group.bench_with_input(BenchmarkId::new("standard_xor", size), &size, |b, &size| {
            let mut data = vec![0x42u8; size];
            let mask = [0x37, 0xfa, 0x21, 0x3d];

            b.iter(|| {
                for (i, byte) in black_box(&mut data).iter_mut().enumerate() {
                    *byte ^= mask[i % 4];
                }
            });
        });
    }

    group.finish();
}

/// Benchmark UTF-8 validation comparison
fn bench_utf8_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("utf8_comparison");

    // ASCII-only strings
    for size in [64, 256, 1024, 4096, 16384] {
        let ascii = "a".repeat(size);
        group.throughput(Throughput::Bytes(size as u64));

        // sockudo-ws UTF-8 validation
        group.bench_with_input(
            BenchmarkId::new("sockudo_ws_ascii", size),
            &ascii,
            |b, data| {
                b.iter(|| sockudo_validate_utf8(black_box(data.as_bytes())));
            },
        );

        // std::str UTF-8 validation
        group.bench_with_input(
            BenchmarkId::new("std_str_ascii", size),
            &ascii,
            |b, data| {
                b.iter(|| std::str::from_utf8(black_box(data.as_bytes())).is_ok());
            },
        );
    }

    // Mixed UTF-8
    for size in [64, 256, 1024, 4096] {
        let mixed = "Hello, 世界! 🎉 ".repeat(size / 20);
        group.throughput(Throughput::Bytes(mixed.len() as u64));

        // sockudo-ws UTF-8 validation
        group.bench_with_input(
            BenchmarkId::new("sockudo_ws_mixed", mixed.len()),
            &mixed,
            |b, data| {
                b.iter(|| sockudo_validate_utf8(black_box(data.as_bytes())));
            },
        );

        // std::str UTF-8 validation
        group.bench_with_input(
            BenchmarkId::new("std_str_mixed", mixed.len()),
            &mixed,
            |b, data| {
                b.iter(|| std::str::from_utf8(black_box(data.as_bytes())).is_ok());
            },
        );
    }

    group.finish();
}

/// Benchmark frame encoding comparison
fn bench_encode_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("encode_comparison");

    for size in [8, 64, 256, 1024, 4096, 16384] {
        let payload: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        group.throughput(Throughput::Bytes(size as u64));

        // sockudo-ws encoding (unmasked - server side)
        group.bench_with_input(
            BenchmarkId::new("sockudo_ws_unmasked", size),
            &payload,
            |b, data| {
                let mut buf = BytesMut::with_capacity(size + 14);
                b.iter(|| {
                    buf.clear();
                    encode_frame(
                        black_box(&mut buf),
                        OpCode::Binary,
                        black_box(data),
                        true,
                        None,
                    );
                });
            },
        );

        // sockudo-ws encoding (masked - client side)
        let mask = [0x37, 0xfa, 0x21, 0x3d];
        group.bench_with_input(
            BenchmarkId::new("sockudo_ws_masked", size),
            &payload,
            |b, data| {
                let mut buf = BytesMut::with_capacity(size + 14);
                b.iter(|| {
                    buf.clear();
                    encode_frame(
                        black_box(&mut buf),
                        OpCode::Binary,
                        black_box(data),
                        true,
                        Some(mask),
                    );
                });
            },
        );
    }

    group.finish();
}

/// Benchmark frame parsing
fn bench_parse_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse_comparison");

    for size in [8, 64, 256, 1024, 4096] {
        let mask = [0x37, 0xfa, 0x21, 0x3d];

        // Create payload
        let payload: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();

        // Encode frame with sockudo-ws
        let mut sockudo_buf = BytesMut::new();
        encode_frame(&mut sockudo_buf, OpCode::Binary, &payload, true, Some(mask));
        let sockudo_frame = sockudo_buf.freeze();

        group.throughput(Throughput::Bytes(sockudo_frame.len() as u64));

        // sockudo-ws parsing
        group.bench_with_input(
            BenchmarkId::new("sockudo_ws", size),
            &sockudo_frame,
            |b, data| {
                let mut parser = SockudoParser::new(1024 * 1024, true);
                b.iter(|| {
                    let mut buf = BytesMut::from(data.as_ref());
                    parser.parse(black_box(&mut buf)).unwrap()
                });
            },
        );
    }

    group.finish();
}

/// Benchmark message encode/decode with protocol layer
fn bench_message_protocol(c: &mut Criterion) {
    let mut group = c.benchmark_group("message_protocol");

    for size in [128, 1024, 8192, 65536] {
        let payload: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        group.throughput(Throughput::Bytes(size as u64));

        // sockudo-ws encode + decode cycle
        group.bench_with_input(
            BenchmarkId::new("sockudo_ws_roundtrip", size),
            &payload,
            |b, data| {
                use sockudo_ws::{
                    Config,
                    protocol::{Message, Protocol, Role},
                };

                b.iter(|| {
                    let config = Config::default();
                    let mut protocol =
                        Protocol::new(Role::Server, config.max_frame_size, config.max_message_size);

                    let mut buf = BytesMut::new();
                    let msg = Message::Binary(Bytes::from(data.clone()));
                    protocol
                        .encode_message(black_box(&msg), black_box(&mut buf))
                        .unwrap();

                    // Parse it back
                    let messages = protocol.process(black_box(&mut buf)).unwrap();
                    black_box(messages);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark handshake key generation
fn bench_handshake(c: &mut Criterion) {
    use sockudo_ws::handshake::{generate_accept_key, generate_key};

    let mut group = c.benchmark_group("handshake");

    group.bench_function("sockudo_ws_generate_key", |b| {
        b.iter(generate_key);
    });

    group.bench_function("sockudo_ws_generate_accept_key", |b| {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        b.iter(|| generate_accept_key(black_box(key)));
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_masking_comparison,
    bench_utf8_comparison,
    bench_encode_comparison,
    bench_parse_comparison,
    bench_message_protocol,
    bench_handshake,
);

criterion_main!(benches);
