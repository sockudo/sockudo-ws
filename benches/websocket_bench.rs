//! Benchmarks for sockudo-ws WebSocket operations
//!
//! Run with: cargo bench

use std::hint::black_box;

use bytes::BytesMut;
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use sockudo_ws::frame::{FrameParser, OpCode, encode_frame};
use sockudo_ws::simd::apply_mask;
use sockudo_ws::utf8::validate_utf8;

/// Benchmark SIMD mask application
fn bench_mask(c: &mut Criterion) {
    let mut group = c.benchmark_group("mask");

    for size in [64, 256, 1024, 4096, 16384, 65536] {
        group.throughput(Throughput::Bytes(size as u64));

        group.bench_with_input(BenchmarkId::new("apply_mask", size), &size, |b, &size| {
            let mut data = vec![0x42u8; size];
            let mask = [0x37, 0xfa, 0x21, 0x3d];

            b.iter(|| {
                apply_mask(black_box(&mut data), black_box(mask));
            });
        });
    }

    group.finish();
}

/// Mask realistic payload offsets relative to a known aligned base.
fn bench_mask_alignment(c: &mut Criterion) {
    let mut group = c.benchmark_group("mask_alignment");
    for size in [0, 8, 16, 32, 63, 64, 65, 125, 128, 256, 1024, 4096] {
        group.throughput(Throughput::Bytes(size as u64));
        for offset in [0, 6, 13] {
            let mut storage = vec![0x42; size + 128];
            let start = storage.as_ptr().align_offset(64) + offset;
            let data = &mut storage[start..start + size];
            let mask = [0x37, 0xfa, 0x21, 0x3d];
            let expected: Vec<_> = data
                .iter()
                .enumerate()
                .map(|(i, value)| value ^ mask[i & 3])
                .collect();
            apply_mask(data, mask);
            assert_eq!(data, expected);

            group.bench_function(BenchmarkId::new(format!("offset_{offset}"), size), |b| {
                b.iter(|| apply_mask(black_box(data), black_box(mask)));
            });
        }
    }
    group.finish();
}

/// Benchmark UTF-8 validation
fn bench_utf8(c: &mut Criterion) {
    let mut group = c.benchmark_group("utf8");

    // ASCII-only strings
    for size in [
        0, 8, 16, 32, 34, 63, 64, 65, 128, 256, 1024, 4096, 8192, 16384,
    ] {
        let ascii = "a".repeat(size);
        group.throughput(Throughput::Bytes(size as u64));

        group.bench_with_input(BenchmarkId::new("ascii", size), &ascii, |b, data| {
            b.iter(|| validate_utf8(black_box(data.as_bytes())));
        });
    }

    // Mixed UTF-8
    for size in [32, 48, 64, 256, 1024, 4096] {
        let mixed = "Hello, 世界! 🎉 ".repeat(size / 20);
        group.throughput(Throughput::Bytes(mixed.len() as u64));

        group.bench_with_input(BenchmarkId::new("mixed", mixed.len()), &mixed, |b, data| {
            b.iter(|| validate_utf8(black_box(data.as_bytes())));
        });
    }

    group.finish();
}

/// Benchmark frame parsing
fn bench_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse");

    for size in [8, 64, 125, 126, 256, 1024, 4096, 65535, 65536] {
        // Create a masked frame
        let mask = [0x37, 0xfa, 0x21, 0x3d];
        let mut buf = BytesMut::new();

        // Create payload
        let payload: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();

        // Encode frame
        encode_frame(&mut buf, OpCode::Binary, &payload, true, Some(mask));

        let frame_data = buf.freeze();
        group.throughput(Throughput::Bytes(frame_data.len() as u64));

        group.bench_with_input(BenchmarkId::new("masked", size), &frame_data, |b, data| {
            let mut parser = FrameParser::new(1024 * 1024, true);
            let mut check = BytesMut::from(data.as_ref());
            assert_eq!(
                parser.parse(&mut check).unwrap().unwrap().payload.as_ref(),
                payload.as_slice()
            );
            assert!(check.is_empty());

            b.iter_batched(
                || BytesMut::from(data.as_ref()),
                |mut buf| parser.parse(black_box(&mut buf)).unwrap(),
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

/// Benchmark frame encoding
fn bench_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("encode");

    for size in [8, 64, 256, 1024, 4096, 16384] {
        let payload: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        group.throughput(Throughput::Bytes(size as u64));

        // Unmasked (server)
        group.bench_with_input(BenchmarkId::new("unmasked", size), &payload, |b, data| {
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
        });

        // Masked (client)
        let mask = [0x37, 0xfa, 0x21, 0x3d];
        group.bench_with_input(BenchmarkId::new("masked", size), &payload, |b, data| {
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
        });
    }

    group.finish();
}

/// Benchmark handshake key generation
fn bench_handshake(c: &mut Criterion) {
    use sockudo_ws::handshake::{generate_accept_key, generate_key};

    let mut group = c.benchmark_group("handshake");

    group.bench_function("generate_key", |b| {
        b.iter(generate_key);
    });

    group.bench_function("generate_accept_key", |b| {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        b.iter(|| generate_accept_key(black_box(key)));
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_mask,
    bench_mask_alignment,
    bench_utf8,
    bench_parse,
    bench_encode,
    bench_handshake,
);

criterion_main!(benches);
