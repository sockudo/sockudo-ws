//! sockudo-ws masking with a simple XOR reference, plus protocol roundtrip work.
//!
//! Run with: cargo bench --bench kernels

use std::hint::black_box;

use bytes::{Bytes, BytesMut};
use criterion::{BenchmarkId, Criterion, Throughput};

// sockudo-ws imports
use sockudo_ws::simd::apply_mask;

/// Benchmark SIMD masking comparison
pub fn bench_masking_comparison(c: &mut Criterion) {
    for tier in ["core", "extended"] {
        let mut group = c.benchmark_group(format!("kernels/{tier}/mask_reference"));

        for size in [64, 256, 1024, 4096, 16384, 65536] {
            group.throughput(Throughput::Bytes(size as u64));

            if (tier == "core") == matches!(size, 64 | 4096 | 65536) {
                // sockudo-ws SIMD masking
                group.bench_with_input(BenchmarkId::new("sockudo_ws", size), &size, |b, &size| {
                    let mut data = vec![0x42u8; size];
                    let mask = [0x37, 0xfa, 0x21, 0x3d];

                    b.iter(|| {
                        apply_mask(black_box(&mut data), black_box(mask));
                    });
                });
            }
            if tier == "extended" {
                // Standard XOR masking (reference implementation)
                group.bench_with_input(
                    BenchmarkId::new("standard_xor", size),
                    &size,
                    |b, &size| {
                        let mut data = vec![0x42u8; size];
                        let mask = [0x37, 0xfa, 0x21, 0x3d];

                        b.iter(|| {
                            let mask = black_box(mask);
                            for (i, byte) in black_box(&mut data).iter_mut().enumerate() {
                                *byte ^= mask[i % 4];
                            }
                        });
                    },
                );
            }
        }
        group.finish();
    }
}

/// Benchmark message encode/decode with protocol layer
pub fn bench_message_protocol(c: &mut Criterion) {
    let mut group = c.benchmark_group("protocol/core/roundtrip");

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

                let config = Config::default();
                let mut sender =
                    Protocol::new(Role::Server, config.max_frame_size, config.max_message_size);
                // The server's unmasked frame must be received in the client role.
                let mut receiver =
                    Protocol::new(Role::Client, config.max_frame_size, config.max_message_size);
                let mut buf = BytesMut::with_capacity(data.len() + 14);
                let msg = Message::Binary(Bytes::copy_from_slice(data));

                sender.encode_message(&msg, &mut buf).unwrap();
                let decoded = receiver.process(&mut buf).unwrap();
                assert_eq!(decoded.len(), 1);
                assert_eq!(decoded[0].as_bytes(), data);
                drop(decoded);

                b.iter(|| {
                    sender
                        .encode_message(black_box(&msg), black_box(&mut buf))
                        .unwrap();

                    // Parse it back
                    let messages = receiver.process(black_box(&mut buf)).unwrap();
                    black_box(messages);
                });
            },
        );
    }

    group.finish();
}
