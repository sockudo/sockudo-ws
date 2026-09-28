//! Benchmarks for sockudo-ws WebSocket operations
//!
//! Run with: cargo bench

use std::hint::black_box;

use bytes::BytesMut;
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput};

use sockudo_ws::frame::{FrameParser, OpCode, encode_frame};
use sockudo_ws::simd::apply_mask;
use sockudo_ws::utf8::validate_utf8;

/// Mask realistic payload offsets relative to a known aligned base.
pub fn bench_mask_alignment(c: &mut Criterion) {
    let mut group = c.benchmark_group("kernels/extended/mask_alignment");
    for size in [
        0, 15, 16, 17, 31, 32, 33, 63, 64, 65, 2047, 2048, 2049, 4096,
    ] {
        group.throughput(if size == 0 {
            Throughput::Elements(1)
        } else {
            Throughput::Bytes(size as u64)
        });
        for offset in [0, 1, 6, 15, 16, 32] {
            // Pair alignment classes with relevant tails instead of expanding every combination.
            if match offset {
                6 => !matches!(size, 63 | 65 | 2047 | 2048 | 2049),
                15 => !matches!(size, 15 | 17 | 2047 | 2048 | 2049),
                16 | 32 => !matches!(size, 31 | 32 | 33 | 63 | 64 | 65 | 2048 | 4096),
                _ => false,
            } {
                continue;
            }
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
pub fn bench_utf8(c: &mut Criterion) {
    for tier in ["core", "extended"] {
        let mut group = c.benchmark_group(format!("kernels/{tier}/utf8"));

        // ASCII-only strings
        for size in [
            0, 8, 16, 32, 34, 63, 64, 65, 128, 256, 1024, 4096, 8192, 16384,
        ] {
            let ascii = "a".repeat(size);
            assert!(validate_utf8(ascii.as_bytes()));
            group.throughput(if size == 0 {
                Throughput::Elements(1)
            } else {
                Throughput::Bytes(size as u64)
            });

            if tier == "extended" {
                group.bench_with_input(BenchmarkId::new("std_ascii", size), &ascii, |b, data| {
                    b.iter(|| std::str::from_utf8(black_box(data.as_bytes())).is_ok());
                });
            }
            if (tier == "core") == matches!(size, 32 | 4096) {
                group.bench_with_input(BenchmarkId::new("ascii", size), &ascii, |b, data| {
                    b.iter(|| validate_utf8(black_box(data.as_bytes())));
                });
            }
        }
        // Mixed UTF-8
        {
            // Match the ASCII byte counts when comparing character distributions.
            for size in [32, 4096] {
                let mixed = "界a".repeat(size / 4);
                assert_eq!(mixed.len(), size);
                assert!(validate_utf8(mixed.as_bytes()));
                group.throughput(Throughput::Bytes(size as u64));
                if tier == "core" {
                    group.bench_with_input(
                        BenchmarkId::new("mixed_equal_bytes", size),
                        &mixed,
                        |b, data| {
                            b.iter(|| validate_utf8(black_box(data.as_bytes())));
                        },
                    );
                } else {
                    group.bench_with_input(
                        BenchmarkId::new("std_mixed_equal_bytes", size),
                        &mixed,
                        |b, data| {
                            b.iter(|| std::str::from_utf8(black_box(data.as_bytes())).is_ok());
                        },
                    );
                }
            }
        }
        for size in [32, 48, 64, 256, 1024, 4096] {
            if tier == "core" {
                continue;
            }
            let mixed = "Hello, 世界! 🎉 ".repeat(size / 20);
            assert!(validate_utf8(mixed.as_bytes()));
            group.throughput(Throughput::Bytes(mixed.len() as u64));

            group.bench_with_input(
                BenchmarkId::new("std_mixed", mixed.len()),
                &mixed,
                |b, data| {
                    b.iter(|| std::str::from_utf8(black_box(data.as_bytes())).is_ok());
                },
            );
            group.bench_with_input(BenchmarkId::new("mixed", mixed.len()), &mixed, |b, data| {
                b.iter(|| validate_utf8(black_box(data.as_bytes())));
            });
        }

        group.finish();
    }
}

/// Benchmark frame parsing
pub fn bench_parse(c: &mut Criterion) {
    for tier in ["core", "extended"] {
        let mut group = c.benchmark_group(format!("kernels/{tier}/parse_prepared"));

        for size in [8, 64, 125, 126, 256, 1024, 4096, 65535, 65536] {
            if (tier == "core") != matches!(size, 32 | 64 | 4096) {
                continue;
            }
            for masked in [false, true] {
                // Create a frame for the receiving role.
                let mask = [0x37, 0xfa, 0x21, 0x3d];
                let mut buf = BytesMut::new();

                // Create payload
                let payload: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();

                // Encode frame
                encode_frame(
                    &mut buf,
                    OpCode::Binary,
                    &payload,
                    true,
                    masked.then_some(mask),
                );

                let frame_data = buf.freeze();
                group.throughput(Throughput::Bytes(frame_data.len() as u64));

                group.bench_with_input(
                    BenchmarkId::new(if masked { "masked" } else { "unmasked" }, size),
                    &frame_data,
                    |b, data| {
                        let mut parser = FrameParser::new(1024 * 1024, masked);
                        let mut check = BytesMut::from(data.as_ref());
                        assert_eq!(
                            parser.parse(&mut check).unwrap().unwrap().payload.as_ref(),
                            payload.as_slice()
                        );
                        assert!(check.is_empty());
                        let mut check = BytesMut::from(data.as_ref());
                        let frame = parser.parse(&mut check).unwrap().unwrap();
                        assert_eq!(frame.header.opcode, OpCode::Binary);
                        assert!(frame.header.fin);
                        assert_eq!(frame.header.masked, masked);

                        b.iter_batched(
                            || BytesMut::from(data.as_ref()),
                            |mut buf| parser.parse(black_box(&mut buf)).unwrap(),
                            BatchSize::SmallInput,
                        );
                    },
                );
            }
        }
        group.finish();
    }
}

/// Benchmark frame encoding
pub fn bench_encode(c: &mut Criterion) {
    for tier in ["core", "extended"] {
        let mut group = c.benchmark_group(format!("kernels/{tier}/encode"));

        for size in [8, 64, 256, 1024, 4096, 16384] {
            if (tier == "core") != matches!(size, 64 | 4096) {
                continue;
            }
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
}

/// Benchmark handshake key generation
pub fn bench_handshake(c: &mut Criterion) {
    use sockudo_ws::handshake::{generate_accept_key, generate_key};

    let mut group = c.benchmark_group("lifecycle/core/keys");

    group.bench_function("generate_key", |b| {
        b.iter(generate_key);
    });

    group.bench_function("generate_accept_key", |b| {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        b.iter(|| generate_accept_key(black_box(key)));
    });

    group.finish();
}
