//! Protocol and cork benchmarks. Input construction is outside the timed body.

use std::hint::black_box;
use std::io::IoSlice;

use bytes::{Bytes, BytesMut};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput};
use sockudo_ws::cork::CorkBuffer;
use sockudo_ws::frame::{OpCode, encode_frame};
use sockudo_ws::protocol::{Protocol, Role};

const MAX_SIZE: usize = 1024 * 1024;
const MASK: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];

pub fn bench_receive_container(c: &mut Criterion) {
    let mut group = c.benchmark_group("protocol/core/output_container");
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

pub fn bench_fragmented_text(c: &mut Criterion) {
    let mut group = c.benchmark_group("protocol/extended/fragmented_steady");
    for size in [4096, 65536] {
        for (kind, payload) in [
            ("ascii", vec![b'a'; size]),
            ("utf8", "a界".repeat(size / 4).into_bytes()),
        ] {
            for fragments in [1, 3, 256] {
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

pub fn bench_write_slices(c: &mut Criterion) {
    let mut group = c.benchmark_group("protocol/extended/cork");
    for chunks in [0, 1, 15, 16, 17] {
        let mut cork = CorkBuffer::with_capacity(16 * 1024);
        cork.write(b"header");
        for _ in 0..chunks {
            cork.write_bytes(Bytes::from(vec![0x42; 8192]));
        }
        // The header is its own segment once a large payload is queued.
        // This fails with the old 4096-byte fixture, which never segmented.
        assert_eq!(cork.get_write_slices().len(), chunks + 1);
        assert_eq!(
            cork.get_write_slices()
                .iter()
                .map(|s| s.len())
                .sum::<usize>(),
            cork.pending_bytes()
        );
        group.bench_function(BenchmarkId::from_parameter(chunks), |b| {
            b.iter(|| {
                let mut slices = [IoSlice::new(&[]); 16];
                let count = black_box(&cork).fill_write_slices(&mut slices);
                black_box(&slices[..count]);
            });
        });
        let expected = [b"header".as_slice(), &vec![0x42; chunks * 8192]].concat();
        let mut actual = Vec::new();
        while cork.has_data() {
            let consumed = {
                let mut slices = [IoSlice::new(&[]); 16];
                let count = cork.fill_write_slices(&mut slices);
                assert_eq!(count, cork.get_write_slices().len().min(16));
                // Consume only a prefix to exercise progress within a segment.
                let amount = slices[0].len().min(4093);
                actual.extend_from_slice(&slices[0][..amount]);
                amount
            };
            cork.consume(consumed);
        }
        assert_eq!(actual, expected);
    }
    group.finish();
}
