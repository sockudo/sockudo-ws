//! Encoding context and threshold costs; outputs are dropped inside timing.
use criterion::{BenchmarkId, Criterion, Throughput};
use rand::{Rng, SeedableRng};
use sockudo_ws::deflate::{DeflateConfig, DeflateDecoder, DeflateEncoder};
use std::hint::black_box;

pub fn benchmark(c: &mut Criterion) {
    json(c);
    for size in [31, 32, 33, 256, 4096, 65536] {
        for reset in [false, true] {
            for random in [false, true] {
                let config = DeflateConfig::default();
                let mut rng = rand::rngs::StdRng::seed_from_u64(42);
                let payloads: Vec<_> = (0..4)
                    .map(|index| {
                        let mut bytes = vec![b'a' + index; size];
                        if random {
                            rng.fill_bytes(&mut bytes);
                        }
                        bytes
                    })
                    .collect();
                let context = if reset { "reset" } else { "takeover" };
                let corpus = if random {
                    "seeded_sequence"
                } else {
                    "repeated"
                };
                let tier = "extended";
                let mut group = c.benchmark_group(format!("deflate/{tier}/encode_{context}"));
                group.throughput(Throughput::Bytes((size * payloads.len()) as u64));
                group.bench_function(BenchmarkId::new(corpus, size), |b| {
                    let mut encoder = DeflateEncoder::new(
                        config.server_max_window_bits,
                        reset,
                        config.compression_level,
                        config.compression_threshold,
                    );
                    let mut decoder = DeflateDecoder::new(config.server_max_window_bits, reset);
                    // Validate successive cycles; takeover inputs cannot be replayed against a fresh decoder.
                    for payload in payloads.iter().cycle().take(12) {
                        if let Some(encoded) = encoder.compress(payload).unwrap() {
                            assert_eq!(
                                decoder.decompress(&encoded, size).unwrap().as_ref(),
                                payload
                            );
                        } else if size < config.compression_threshold {
                            assert!(size < 32);
                        }
                    }
                    b.iter(|| {
                        for payload in &payloads {
                            black_box(encoder.compress(black_box(payload)).unwrap());
                        }
                    });
                });
                group.finish();
            }
        }
    }
    for (name, config) in [
        ("default", DeflateConfig::default()),
        ("low_memory", DeflateConfig::low_memory()),
    ] {
        let payloads = crate::corpus::json_messages(4096, 16);
        let mut group = c.benchmark_group("deflate/extended/configuration");
        group.throughput(Throughput::Bytes(4096));
        group.bench_function(name, |b| {
            let mut encoder = DeflateEncoder::new(
                config.server_max_window_bits,
                config.server_no_context_takeover,
                config.compression_level,
                config.compression_threshold,
            );
            let mut decoder = DeflateDecoder::new(
                config.server_max_window_bits,
                config.server_no_context_takeover,
            );
            // Compare complete presets (window, takeover, level and threshold), not one knob.
            for payload in &payloads {
                assert_eq!(
                    decoder
                        .decompress(&encoder.compress(payload).unwrap().unwrap(), payload.len())
                        .unwrap()
                        .as_ref(),
                    payload.as_ref()
                );
            }
            let mut index = 0;
            b.iter(|| {
                let encoded = encoder.compress(black_box(&payloads[index])).unwrap();
                index = (index + 1) % payloads.len();
                black_box(encoded)
            });
        });
        group.finish();
    }
}

fn json(c: &mut Criterion) {
    use sockudo_ws::deflate::MAX_WINDOW_BITS;
    for size in [256, 4096] {
        for reset in [false, true] {
            // Share the proof across selected encode/decode cases, without work during listing.
            let fixture = std::cell::OnceCell::new();
            let prepare = || {
                fixture.get_or_init(|| {
                    let payloads = crate::corpus::json_messages(size, 65536 / size);
                    let (first, repeated) = crate::corpus::compressed_cycles(&payloads, reset);
                    (payloads, first, repeated)
                })
            };
            let context = if reset { "reset" } else { "takeover" };
            let mut group = c.benchmark_group(format!("deflate/core/json_{context}"));
            group.throughput(Throughput::Bytes(size as u64));
            group.bench_function(BenchmarkId::new("compress", size), |b| {
                let (payloads, _, _) = prepare();
                let mut encoder = DeflateEncoder::new(MAX_WINDOW_BITS, reset, 6, 32);
                for payload in payloads {
                    black_box(encoder.compress(payload).unwrap().unwrap());
                }
                let mut index = 0;
                b.iter(|| {
                    let encoded = encoder
                        .compress(black_box(&payloads[index]))
                        .unwrap()
                        .unwrap();
                    index = (index + 1) % payloads.len();
                    black_box(encoded)
                });
            });
            group.bench_function(BenchmarkId::new("decompress", size), |b| {
                let (payloads, first, repeated) = prepare();
                let mut decoder = DeflateDecoder::new(MAX_WINDOW_BITS, reset);
                for (wire, expected) in first.iter().zip(payloads) {
                    assert_eq!(
                        decoder.decompress(wire, size).unwrap().as_ref(),
                        expected.as_ref()
                    );
                }
                let mut index = 0;
                b.iter(|| {
                    let decoded = decoder
                        .decompress(black_box(&repeated[index]), size)
                        .unwrap();
                    index = (index + 1) % repeated.len();
                    black_box(decoded)
                });
                // Continue from the exact phase reached by Criterion, not from an arbitrary reset.
                for _ in 0..repeated.len() * 2 {
                    assert_eq!(
                        decoder.decompress(&repeated[index], size).unwrap().as_ref(),
                        payloads[index].as_ref()
                    );
                    index = (index + 1) % repeated.len();
                }
            });
            group.finish();
        }
    }
    let mut random = vec![0; 4096];
    rand::rngs::StdRng::seed_from_u64(42).fill_bytes(&mut random);
    let mut group = c.benchmark_group("deflate/core/random_reset");
    group.throughput(Throughput::Bytes(random.len() as u64));
    group.bench_function("4096", |b| {
        let mut encoder = DeflateEncoder::new(MAX_WINDOW_BITS, true, 6, 32);
        if let Some(wire) = encoder.compress(&random).unwrap() {
            let mut decoder = DeflateDecoder::new(MAX_WINDOW_BITS, true);
            assert_eq!(
                decoder.decompress(&wire, random.len()).unwrap().as_ref(),
                random
            );
        }
        b.iter(|| black_box(encoder.compress(black_box(&random)).unwrap()));
    });
    group.finish();
}
