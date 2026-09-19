//! Protocol and cork benchmarks. Input construction is outside the timed body.

use std::hint::black_box;

use bytes::{Bytes, BytesMut};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use sockudo_ws::cork::CorkBuffer;
use sockudo_ws::frame::{OpCode, encode_frame};
use sockudo_ws::protocol::{Protocol, Role};

const MAX_SIZE: usize = 1024 * 1024;
const MASK: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];

fn bench_receive_container(c: &mut Criterion) {
    let mut group = c.benchmark_group("receive_container");
    for count in [1, 16, 128] {
        let payload = [0x42; 32];
        let mut wire = BytesMut::new();
        for _ in 0..count {
            encode_frame(&mut wire, OpCode::Binary, &payload, true, Some(MASK));
        }
        let mut protocol = Protocol::new(Role::Server, MAX_SIZE, MAX_SIZE);
        let mut check = wire.clone();
        let decoded = protocol.process(&mut check).unwrap();
        assert_eq!(decoded.len(), count);
        assert!(decoded.iter().all(|message| message.as_bytes() == payload));
        assert!(check.is_empty());
        group.throughput(Throughput::Elements(count as u64));

        group.bench_function(BenchmarkId::new("process", count), |b| {
            b.iter_batched(
                || wire.clone(),
                |mut input| {
                    let messages = protocol.process(black_box(&mut input)).unwrap();
                    black_box(&messages);
                    drop(messages);
                },
                BatchSize::SmallInput,
            );
        });

        let mut messages = Vec::with_capacity(count);
        group.bench_function(BenchmarkId::new("process_into", count), |b| {
            b.iter_batched(
                || wire.clone(),
                |mut input| {
                    protocol
                        .process_into(black_box(&mut input), &mut messages)
                        .unwrap();
                    black_box(&messages);
                    messages.clear();
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_fragmented_text(c: &mut Criterion) {
    let mut group = c.benchmark_group("fragmented_text");
    for size in [4096, 65536] {
        for (kind, payload) in [
            ("ascii", vec![b'a'; size]),
            ("utf8", "a界".repeat(size / 4).into_bytes()),
        ] {
            for fragments in [1, 2, 3, 16, 256] {
                let mut wire = BytesMut::new();
                // Non-dividing fragment counts split multibyte characters too.
                for index in 0..fragments {
                    let start = index * size / fragments;
                    let end = (index + 1) * size / fragments;
                    encode_frame(
                        &mut wire,
                        if index == 0 {
                            OpCode::Text
                        } else {
                            OpCode::Continuation
                        },
                        &payload[start..end],
                        index + 1 == fragments,
                        Some(MASK),
                    );
                }
                let mut protocol = Protocol::new(Role::Server, MAX_SIZE, MAX_SIZE);
                let mut check = wire.clone();
                let decoded = protocol.process(&mut check).unwrap();
                assert_eq!(decoded.len(), 1);
                assert_eq!(decoded[0].as_bytes(), payload);
                assert!(check.is_empty());
                drop(decoded);
                let mut messages = Vec::with_capacity(1);
                group.throughput(Throughput::Bytes(size as u64));
                group.bench_function(BenchmarkId::new(format!("{kind}/{size}"), fragments), |b| {
                    b.iter_batched(
                        || wire.clone(),
                        |mut input| {
                            protocol
                                .process_into(black_box(&mut input), &mut messages)
                                .unwrap();
                            black_box(&messages);
                            messages.clear();
                        },
                        BatchSize::SmallInput,
                    );
                });
            }
        }
    }
    group.finish();
}

fn bench_write_slices(c: &mut Criterion) {
    let mut group = c.benchmark_group("cork_slices");
    for chunks in [0, 1, 16] {
        let mut cork = CorkBuffer::with_capacity(16 * 1024);
        cork.write(b"header");
        for _ in 0..chunks {
            cork.write_bytes(Bytes::from(vec![0x42; 4096]));
        }
        assert_eq!(
            cork.get_write_slices()
                .iter()
                .map(|s| s.len())
                .sum::<usize>(),
            cork.pending_bytes()
        );
        group.bench_function(BenchmarkId::from_parameter(chunks), |b| {
            b.iter(|| black_box(black_box(&cork).get_write_slices()));
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_receive_container,
    bench_fragmented_text,
    bench_write_slices
);
criterion_main!(benches);
