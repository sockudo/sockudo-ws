//! Compression and fan-out baselines; returned output and queue consumption are timed.

use std::hint::black_box;
use std::sync::{Arc, Barrier};
use std::time::Instant;

#[cfg(feature = "tokio-runtime")]
use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput};
#[cfg(feature = "permessage-deflate")]
use rand::{Rng, SeedableRng};
#[cfg(feature = "tokio-runtime")]
use sockudo_ws::Message;
#[cfg(feature = "permessage-deflate")]
use sockudo_ws::SharedCompressorPool;
#[cfg(feature = "permessage-deflate")]
use sockudo_ws::deflate::{DeflateConfig, DeflateDecoder, DeflateEncoder};
#[cfg(feature = "tokio-runtime")]
use sockudo_ws::pubsub::PubSub;

#[cfg(feature = "permessage-deflate")]
pub fn bench_deflate(c: &mut Criterion) {
    let mut group = c.benchmark_group("deflate/extended/reset");
    for size in [256, 4096, 65536] {
        let text = b"a repeated message with text and numbers 0123456789 ";
        let payload: Vec<_> = text.iter().copied().cycle().take(size).collect();
        // A fixed PRNG seed makes the incompressible corpus reproducible.
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let mut random = vec![0; size];
        rng.fill_bytes(&mut random);
        group.throughput(Throughput::Bytes(size as u64));
        for (kind, input) in [("text", payload), ("random", random)] {
            let mut encoder = DeflateEncoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, true, 6, 0);
            let compressed = encoder.compress(&input).unwrap();
            if !(kind == "random" && size == 4096) {
                group.bench_function(BenchmarkId::new(format!("compress/{kind}"), size), |b| {
                    b.iter(|| black_box(encoder.compress(black_box(&input)).unwrap()));
                });
            }
            if let Some(compressed) = compressed {
                let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, true);
                assert_eq!(
                    decoder.decompress(&compressed, size).unwrap().as_ref(),
                    input
                );
                group.bench_function(BenchmarkId::new(format!("decompress/{kind}"), size), |b| {
                    b.iter(|| black_box(decoder.decompress(black_box(&compressed), size).unwrap()));
                });
            }
        }
    }
    group.finish();
}

#[cfg(feature = "tokio-runtime")]
pub fn bench_publish(c: &mut Criterion) {
    let mut group = c.benchmark_group("pubsub/core/publish_drain");
    for recipients in [1, 100, 1000] {
        let pubsub = PubSub::new();
        let mut receivers = Vec::new();
        for _ in 0..recipients {
            let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
            let subscriber = pubsub.create_subscriber(sender);
            assert!(pubsub.subscribe(subscriber, "updates"));
            receivers.push(receiver);
        }
        let message = Message::Binary(Bytes::from(vec![0x42; 256]));
        assert_eq!(
            pubsub.publish("updates", message.clone()).count(),
            recipients
        );
        for receiver in &mut receivers {
            assert_eq!(receiver.try_recv().unwrap().as_bytes(), message.as_bytes());
        }
        group.throughput(Throughput::Elements(recipients as u64));
        group.bench_function(BenchmarkId::from_parameter(recipients), |b| {
            b.iter(|| {
                black_box(pubsub.publish("updates", message.clone()));
                // Drain each iteration so the benchmark cannot grow an unbounded backlog.
                for receiver in &mut receivers {
                    black_box(receiver.try_recv().unwrap());
                }
            });
        });
    }
    group.finish();
}

#[cfg(feature = "permessage-deflate")]
pub fn bench_shared_compression(c: &mut Criterion) {
    let mut group = c.benchmark_group("deflate/extended/shared_pool");
    let payload = Arc::new(
        b"shared compressor contention payload with repeated text "
            .iter()
            .copied()
            .cycle()
            .take(4096)
            .collect::<Vec<_>>(),
    );

    for workers in [1, 4, 8, 16] {
        for shared in [false, true] {
            let pool = Arc::new(SharedCompressorPool::new(DeflateConfig::default()));
            let compressed = pool
                .compress(&payload)
                .unwrap()
                .expect("repeated payload must compress");
            let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, true);
            assert_eq!(
                decoder
                    .decompress(&compressed, payload.len())
                    .unwrap()
                    .as_ref(),
                payload.as_slice()
            );
            drop(compressed);
            group.throughput(Throughput::Elements(workers as u64));
            group.bench_function(
                BenchmarkId::new(if shared { "shared" } else { "dedicated" }, workers),
                |b| {
                    b.iter_custom(|iterations| {
                        std::thread::scope(|scope| {
                            let ready = Arc::new(Barrier::new(workers + 1));
                            let mut handles = Vec::with_capacity(workers);
                            for _ in 0..workers {
                                let pool = Arc::clone(&pool);
                                let payload = Arc::clone(&payload);
                                let ready = Arc::clone(&ready);
                                handles.push(scope.spawn(move || {
                                    let config = DeflateConfig::default();
                                    let mut encoder = DeflateEncoder::new(
                                        config.server_max_window_bits,
                                        true,
                                        config.compression_level,
                                        config.compression_threshold,
                                    );
                                    let encoded = encoder.compress(&payload).unwrap().unwrap();
                                    let mut decoder =
                                        DeflateDecoder::new(config.server_max_window_bits, true);
                                    assert_eq!(
                                        decoder
                                            .decompress(&encoded, payload.len())
                                            .unwrap()
                                            .as_ref(),
                                        payload.as_slice()
                                    );
                                    ready.wait();
                                    ready.wait();
                                    if shared {
                                        for _ in 0..iterations {
                                            black_box(pool.compress(black_box(&payload)).unwrap());
                                        }
                                    } else {
                                        for _ in 0..iterations {
                                            black_box(
                                                encoder.compress(black_box(&payload)).unwrap(),
                                            );
                                        }
                                    }
                                }));
                            }

                            // All workers finish allocation and validation before the timed release.
                            ready.wait();
                            let start = Instant::now();
                            ready.wait();
                            for handle in handles {
                                handle.join().unwrap();
                            }
                            start.elapsed()
                        })
                    });
                },
            );
        }
    }
    group.finish();
}

#[cfg(feature = "tokio-runtime")]
pub fn bench_publish_with_churn(c: &mut Criterion) {
    let mut group = c.benchmark_group("pubsub/extended/finite_churn");
    group.throughput(Throughput::Elements(1));
    group.bench_function("one_recipient", |b| {
        b.iter_custom(|iterations| {
            let pubsub = Arc::new(PubSub::new());
            let (stable_sender, mut stable_receiver) = tokio::sync::mpsc::unbounded_channel();
            let stable_id = pubsub.create_subscriber(stable_sender);
            assert!(pubsub.subscribe(stable_id, "updates"));
            let (churn_sender, _churn_receiver) = tokio::sync::mpsc::unbounded_channel();
            let churn_id = pubsub.create_subscriber(churn_sender);
            let message = Message::Binary(Bytes::from_static(b"update"));

            std::thread::scope(|scope| {
                let ready = Arc::new(Barrier::new(2));
                let churn_pubsub = Arc::clone(&pubsub);
                let churn_ready = Arc::clone(&ready);
                let churn = scope.spawn(move || {
                    churn_ready.wait();
                    for _ in 0..iterations {
                        assert!(churn_pubsub.subscribe(churn_id, "churn"));
                        assert!(churn_pubsub.unsubscribe(churn_id, "churn"));
                    }
                });

                let start = Instant::now();
                ready.wait();
                for _ in 0..iterations {
                    black_box(pubsub.publish("updates", message.clone()));
                    black_box(stable_receiver.try_recv().unwrap());
                }
                churn.join().unwrap();
                start.elapsed()
            })
        });
    });
    group.finish();
}
