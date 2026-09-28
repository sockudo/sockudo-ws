//! Publish selection including empty fanout, with bounded queue consumption.
use criterion::{BenchmarkId, Criterion, Throughput};
use sockudo_ws::{Message, pubsub::PubSub};
use std::hint::black_box;

pub fn benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("pubsub/core/selection");
    group.throughput(Throughput::Elements(1));
    for recipients in [0, 1, 100, 1000] {
        for excluding in [false, true] {
            // Ordinary nonempty publish-and-drain is already owned by services_bench.
            if !excluding && recipients != 0 {
                continue;
            }
            group.bench_function(
                BenchmarkId::new(if excluding { "exclude" } else { "empty" }, recipients),
                |b| {
                    let pubsub = PubSub::new();
                    let (sender, mut excluded) = tokio::sync::mpsc::unbounded_channel();
                    let exclude = pubsub.create_subscriber(sender);
                    if excluding {
                        assert!(pubsub.subscribe(exclude, "updates"));
                    }
                    let mut receivers = Vec::new();
                    for _ in 0..recipients {
                        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
                        assert!(pubsub.subscribe(pubsub.create_subscriber(sender), "updates"));
                        receivers.push(receiver);
                    }
                    let message = Message::binary(vec![0x42; 256]);
                    let mut publish = |verify: bool| {
                        let result = if excluding {
                            pubsub.publish_excluding(exclude, "updates", message.clone())
                        } else {
                            pubsub.publish("updates", message.clone())
                        };
                        for receiver in &mut receivers {
                            let received = receiver.try_recv().unwrap();
                            if verify {
                                assert_eq!(received.as_bytes(), message.as_bytes());
                            } else {
                                black_box(received);
                            }
                        }
                        if verify {
                            assert!(excluded.try_recv().is_err());
                        }
                        result.count()
                    };
                    assert_eq!(publish(true), recipients);
                    b.iter(|| black_box(publish(false)));
                    assert_eq!(publish(true), recipients);
                },
            );
        }
    }
    group.finish();
}

/// Report observed churn progress as well as elapsed publication time.
/// Zero progress means that sample did not exercise concurrent subscription work.
pub fn concurrent(c: &mut Criterion) {
    use std::sync::{
        Barrier,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use std::time::Instant;
    for same_topic in [false, true] {
        let mut group = c.benchmark_group("pubsub/extended/continuous_tracked_churn");
        group.throughput(Throughput::Elements(1024));
        group.bench_function(if same_topic {"same_topic"} else {"other_topic"}, |b| {
            let pubsub = PubSub::new();
            let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
            assert!(pubsub.subscribe(pubsub.create_subscriber(sender), "updates"));
            let (sender, mut changing_receiver) = tokio::sync::mpsc::unbounded_channel();
            let changing = pubsub.create_subscriber(sender);
            let message = Message::binary(vec![0x42; 256]);
            let mut min_cycles = usize::MAX;
            b.iter_custom(|iterations| {
                let done = AtomicBool::new(false);
                let cycles = AtomicUsize::new(0);
                let ready = Barrier::new(2);
                std::thread::scope(|scope| {
                let worker = scope.spawn(|| {
                    let topic = if same_topic {"updates"} else {"churn"};
                    ready.wait();
                    while !done.load(Ordering::Relaxed) {
                        assert!(pubsub.subscribe(changing, topic));
                        assert!(pubsub.unsubscribe(changing, topic));
                        cycles.fetch_add(1, Ordering::Relaxed);
                    }
                });
                ready.wait();
                let before = cycles.load(Ordering::Relaxed);
                let start = Instant::now();
                for _ in 0..iterations {
                    for _ in 0..1024 {
                        black_box(pubsub.publish("updates", message.clone()));
                        black_box(receiver.try_recv().unwrap());
                        // At most one publication can be queued between these drains.
                        if let Ok(message) = changing_receiver.try_recv() { black_box(message); }
                    }
                }
                let elapsed = start.elapsed();
                min_cycles = min_cycles.min(cycles.load(Ordering::Relaxed) - before);
                done.store(true, Ordering::Relaxed);
                worker.join().unwrap();
                assert!(receiver.try_recv().is_err());
                assert!(changing_receiver.try_recv().is_err());
                elapsed
                })
            });
            eprintln!("pubsub churn same_topic={same_topic}: minimum completed cycles during a measured batch={min_cycles}");
        });
        group.finish();
    }
}
