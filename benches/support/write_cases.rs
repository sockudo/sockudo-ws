//! Local send completion into a controlled sink. Capture/decoding runs outside timing.
use super::controlled_io::{Output, Written};
use bytes::{Bytes, BytesMut};
use rand::{Rng, SeedableRng};
use sockudo_ws::{Config, Message, Role, protocol::Protocol};
use std::sync::{Arc, Mutex};

pub struct Case {
    pub name: &'static str,
    pub size: usize,
    pub batch: usize,
    pub limit: usize,
    pub vectored: bool,
    pub pending: bool,
    pub client: bool,
    pub timers: bool,
}
pub fn cases(segmented: bool, compressed: bool, feed: bool, unified: bool) -> Vec<Case> {
    let make = |name, size, batch, limit, vectored, pending, client, timers| Case {
        name,
        size,
        batch,
        limit,
        vectored,
        pending,
        client,
        timers,
    };
    if feed {
        return vec![make(
            "extended/batch16",
            8192,
            16,
            usize::MAX,
            true,
            false,
            false,
            false,
        )];
    }
    let small = 32;
    let mut cases = vec![
        make(
            "core/small_timers_enabled",
            small,
            1,
            usize::MAX,
            true,
            false,
            false,
            true,
        ),
        make(
            "core/client_small",
            small,
            1,
            usize::MAX,
            false,
            false,
            true,
            true,
        ),
        make(
            "extended/timers_off",
            small,
            1,
            usize::MAX,
            true,
            false,
            false,
            false,
        ),
        make(
            "extended/large",
            65536,
            1,
            usize::MAX,
            true,
            false,
            false,
            false,
        ),
        make(
            "extended/client_large",
            8192,
            1,
            usize::MAX,
            false,
            false,
            true,
            false,
        ),
    ];
    for (name, pending) in [
        ("extended/partial", false),
        ("extended/partial_pending", true),
    ] {
        cases.push(make(name, 8192, 1, 4093, true, pending, false, false));
    }
    if segmented && !compressed {
        for (name, size, vectored) in [
            ("extended/below_segment", 8191, true),
            ("core/segment_timers_off", 8192, true),
            ("extended/above_segment", 8193, true),
            ("extended/scalar", 8192, false),
        ] {
            cases.push(make(
                name,
                size,
                1,
                usize::MAX,
                vectored,
                false,
                false,
                false,
            ));
        }
    }
    if compressed && unified {
        cases.push(make(
            "core/json_records",
            256,
            1,
            usize::MAX,
            true,
            false,
            false,
            true,
        ));
        for size in [31, 33] {
            cases.push(make(
                "extended/threshold",
                size,
                1,
                usize::MAX,
                true,
                false,
                false,
                false,
            ));
        }
    }
    cases
}
impl Case {
    pub fn role(&self) -> Role {
        if self.client {
            Role::Client
        } else {
            Role::Server
        }
    }
    pub fn config(&self) -> Config {
        Config::builder()
            .auto_ping(self.timers)
            .ping_interval(3600)
            .idle_timeout(if self.timers { 3600 } else { 0 })
            .max_backpressure(usize::MAX)
            .build()
    }
    pub fn output(&self) -> Output {
        Output::new(self.limit, self.vectored, self.pending)
    }
    pub fn messages(&self, compressed: bool) -> Vec<Message> {
        if self.limit != usize::MAX {
            let mut rng = rand::rngs::StdRng::seed_from_u64(42);
            // Exceed the dictionary with independent messages, so warm cycles still short-write.
            (0..8)
                .map(|_| {
                    let mut payload = vec![0; self.size];
                    rng.fill_bytes(&mut payload);
                    Message::Binary(Bytes::from(payload))
                })
                .collect()
        } else if self.size <= 33 {
            let mut rng = rand::rngs::StdRng::seed_from_u64(42);
            // Shared plain/compressed control: no whole-message repeats within the dictionary.
            (0..65536usize.div_ceil(self.size))
                .map(|index| {
                    let mut payload = vec![0; self.size];
                    rng.fill_bytes(&mut payload);
                    payload[..8].copy_from_slice(&(index as u64).to_le_bytes());
                    Message::Binary(Bytes::from(payload))
                })
                .collect()
        } else if compressed && self.size >= 256 {
            crate::corpus::json_messages(self.size, (65536 / self.size).max(2))
                .into_iter()
                .map(Message::Text)
                .collect()
        } else {
            vec![Message::Binary(Bytes::from(vec![0x42; self.size]))]
        }
    }
    pub fn verify(
        &self,
        trace: &Written,
        expected: &[Message],
        compressed: bool,
        segmented: bool,
        feed: bool,
    ) {
        let mut input = BytesMut::from(trace.bytes.as_slice());
        if compressed {
            let mut wire = input.clone();
            let mut parser = sockudo_ws::frame::FrameParser::with_compression(1 << 20, self.client);
            let mut count = 0;
            while let Some(frame) = parser.parse(&mut wire).unwrap() {
                assert_eq!(
                    frame.header.rsv1,
                    self.size >= 32,
                    "compression threshold must select the expected wire path"
                );
                count += 1;
            }
            assert!(wire.is_empty());
            assert_eq!(count, expected.len());
        }
        let messages = if compressed {
            #[cfg(feature = "permessage-deflate")]
            {
                let mut protocol = if self.client {
                    sockudo_ws::protocol::CompressedProtocol::server(
                        1 << 20,
                        1 << 20,
                        sockudo_ws::DeflateConfig::default(),
                    )
                } else {
                    sockudo_ws::protocol::CompressedProtocol::client(
                        1 << 20,
                        1 << 20,
                        sockudo_ws::DeflateConfig::default(),
                    )
                };
                protocol.process(&mut input).unwrap()
            }
            #[cfg(not(feature = "permessage-deflate"))]
            unreachable!("compressed case requires permessage-deflate")
        } else {
            let mut protocol = Protocol::new(
                if self.client {
                    Role::Server
                } else {
                    Role::Client
                },
                1 << 20,
                1 << 20,
            );
            protocol.process(&mut input).unwrap()
        };
        assert!(input.is_empty());
        assert_eq!(messages.len(), expected.len());
        for (actual, expected) in messages.iter().zip(expected) {
            assert_eq!(actual.as_bytes(), expected.as_bytes());
        }
        if segmented && !compressed && !self.client && self.size >= 8192 && self.vectored {
            assert!(trace.vectored > 0);
            if feed && self.batch == 16 {
                assert_eq!(trace.max_slices, 16);
            }
        } else if !self.vectored || (segmented && !compressed && !self.client && self.size < 8192) {
            assert_eq!(trace.vectored, 0);
        }
        if self.pending {
            assert!(trace.pending > 0);
        }
    }
}
pub fn capture(output: &mut Output) -> Arc<Mutex<Written>> {
    let trace = Arc::new(Mutex::new(Written::default()));
    output.trace = Some(trace.clone());
    trace
}

macro_rules! write_case {
    (@flush $writer:ident, send) => {};
    (@flush $writer:ident, feed) => { $writer.flush().await.unwrap(); };
    ($c:expr, $runtime:expr, $backend:expr, $kind:expr, $method:ident, $make:expr) => {
        let feed = stringify!($method) == "feed";
        for case in crate::write_cases::cases($kind.segmented($backend), $kind.compressed(), feed, $kind.unified()) {
            let mut group = $c.benchmark_group(format!("{}/{}/{}_{}", $backend.suite(), case.name, $kind.name(),stringify!($method)));
            group.throughput(criterion::Throughput::Elements(case.batch as u64));
            // Lazy once-per-ID setup preserves Criterion filtering and fresh per-sample streams.
            let fixture = std::cell::OnceCell::new();
            group.bench_function(case.size.to_string(), |b| {
                let runtime = $runtime;
                let messages = fixture.get_or_init(|| runtime.block_on(async {
                    let messages = case.messages($kind.compressed());
                    // A separate connection keeps capture locks and dictionary verification out of timing.
                    let mut output = case.output();
                    let trace = crate::write_cases::capture(&mut output);
                    let (mut writer, guard) = ($make)(output,&case);
                    let mut expected = Vec::new();
                    for _ in 0..2 {
                        for message in messages.iter().cycle().take(messages.len().max(case.batch)) {
                            let before = { let t = trace.lock().unwrap(); (t.short_writes, t.scalar + t.vectored) };
                            std::hint::black_box(&mut writer).$method(message.clone()).await.unwrap();
                            if case.limit != usize::MAX {
                                let t = trace.lock().unwrap();
                                assert!(t.short_writes > before.0, "each cold/warm message must short-write");
                                assert!((2..=4).contains(&(t.scalar + t.vectored - before.1)), "partial is a few writes, not a polling stress case");
                            }
                            expected.push(message.clone());
                        }
                        write_case!(@flush writer, $method);
                    }
                    case.verify(&trace.lock().unwrap(), &expected, $kind.compressed(), $kind.segmented($backend), feed);
                    drop((writer,guard));
                    messages
                }));
                let (mut writer, guard) = runtime.block_on(async { ($make)(case.output(),&case) });
                let mut index = 0;
                b.iter_custom(|iterations| runtime.block_on(async {
                    let start = std::time::Instant::now();
                    for _ in 0..iterations {
                        for _ in 0..case.batch {
                            let message = messages[index].clone();
                            index = (index + 1) % messages.len();
                            std::hint::black_box(&mut writer).$method(std::hint::black_box(message)).await.unwrap();
                        }
                        write_case!(@flush writer, $method);
                    }
                    start.elapsed()
                }));
                runtime.block_on(async { drop((writer,guard)); });
            });
            group.finish();
        }
    };
}
pub(crate) use write_case;
