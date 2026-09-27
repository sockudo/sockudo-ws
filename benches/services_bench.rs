//! Compression and fan-out baselines; returned output and queue consumption are timed.

use std::hint::black_box;
use std::sync::{Arc, Barrier};
use std::time::Instant;

use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use rand::{RngCore, SeedableRng};
use sockudo_ws::Message;
use sockudo_ws::SharedCompressorPool;
use sockudo_ws::deflate::{DeflateConfig, DeflateDecoder, DeflateEncoder};
use sockudo_ws::pubsub::PubSub;

fn bench_deflate(c: &mut Criterion) {
    let mut group = c.benchmark_group("deflate");
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
            group.bench_function(BenchmarkId::new(format!("compress/{kind}"), size), |b| {
                b.iter(|| black_box(encoder.compress(black_box(&input)).unwrap()));
            });
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

fn bench_publish(c: &mut Criterion) {
    let mut group = c.benchmark_group("pubsub_publish_and_drain");
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

fn bench_shared_compression(c: &mut Criterion) {
    let mut group = c.benchmark_group("shared_compression_concurrency");
    let payload = Arc::new(
        b"shared compressor contention payload with repeated text "
            .iter()
            .copied()
            .cycle()
            .take(4096)
            .collect::<Vec<_>>(),
    );

    for workers in [1, 4, 8, 16] {
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
        group.bench_function(BenchmarkId::from_parameter(workers), |b| {
            b.iter_custom(|iterations| {
                std::thread::scope(|scope| {
                    let ready = Arc::new(Barrier::new(workers + 1));
                    let mut handles = Vec::with_capacity(workers);
                    for _ in 0..workers {
                        let pool = Arc::clone(&pool);
                        let payload = Arc::clone(&payload);
                        let ready = Arc::clone(&ready);
                        handles.push(scope.spawn(move || {
                            ready.wait();
                            for _ in 0..iterations {
                                black_box(pool.compress(black_box(&payload)).unwrap());
                            }
                        }));
                    }

                    let start = Instant::now();
                    ready.wait();
                    for handle in handles {
                        handle.join().unwrap();
                    }
                    start.elapsed()
                })
            });
        });
    }
    group.finish();
}

fn bench_publish_with_churn(c: &mut Criterion) {
    let mut group = c.benchmark_group("pubsub_publish_with_churn");
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

criterion_group!(
    benches,
    bench_deflate,
    bench_publish,
    bench_shared_compression,
    bench_publish_with_churn
);
criterion_main!(benches);
