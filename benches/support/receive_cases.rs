//! One iteration is a complete, finite burst; first-delivery samples time only next().
use super::controlled_io::{Input, Read};
use super::stream_cases::{Kind, Runtime};
use bytes::{Bytes, BytesMut};
use sockudo_ws::{
    Config, Message,
    frame::{OpCode, encode_frame},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Cut {
    Header,
    Body,
}

pub struct Case {
    pub name: &'static str,
    pub size: usize,
    pub batch: usize,
    pub first: bool,
    pub read_limit: usize,
    pub pending: bool,
    pub retain: usize,
    pub timers: bool,
    pub server: bool,
    pub text: bool,
    pub validate_text_utf8: bool,
    pub window: bool,
    pub control: bool,
    pub cut: Option<Cut>,
    pub takeover: bool,
    pub spare: Option<usize>,
}

pub fn cases(runtime: Runtime, kind: Kind) -> Vec<Case> {
    let compressed = kind.compressed();
    let make = |name, size, batch, server, text, timers| Case {
        name,
        size,
        batch,
        first: false,
        read_limit: usize::MAX,
        pending: false,
        retain: 0,
        timers,
        server,
        text,
        validate_text_utf8: true,
        window: false,
        control: false,
        cut: None,
        takeover: false,
        spare: None,
    };
    let mut cases = vec![
        make("core/single", 32, 1, false, false, true),
        make("core/server_small", 32, 1, true, false, true),
        Case {
            first: true,
            ..make("core/burst", 256, 16, true, true, true)
        },
        make("core/binary_burst", 256, 16, true, false, true),
        make(
            if kind == Kind::PlainUnified {
                "core/large_timers_off"
            } else {
                "extended/large"
            },
            65536,
            1,
            false,
            false,
            false,
        ),
        Case {
            first: true,
            ..make("extended/burst128", 32, 128, false, false, false)
        },
        make("extended/timers_off", 32, 1, true, false, false),
    ];
    for (name, enabled) in [
        ("extended/text_utf8_on", true),
        ("extended/text_utf8_off", false),
    ] {
        for size in [256, 4096] {
            let mut case = make(name, size, 16, false, true, false);
            case.validate_text_utf8 = enabled;
            cases.push(case);
        }
    }
    for (name, limit, pending, cut) in [
        ("extended/read1", 1, false, None),
        (
            "extended/split_header",
            usize::MAX,
            false,
            Some(Cut::Header),
        ),
        ("extended/split_body", usize::MAX, false, Some(Cut::Body)),
        ("extended/pending", usize::MAX, true, None),
    ] {
        let mut case = make(name, 256, 1, true, false, false);
        case.read_limit = limit;
        case.pending = pending;
        case.cut = cut;
        cases.push(case);
    }
    if runtime == Runtime::Tokio && !compressed {
        for wire_size in [24576, 32767, 32768, 40960] {
            let mut case = make("extended/window_fit", wire_size - 4, 1, false, false, false);
            case.window = true;
            cases.push(case);
        }
        let mut retained = make(
            "extended/window_retained",
            40960 - 4,
            1,
            false,
            false,
            false,
        );
        retained.window = true;
        retained.retain = 16;
        cases.push(retained);
    }
    if runtime == Runtime::Compio && kind == Kind::PlainUnified {
        for spare in [4095, 4096, 4097] {
            let mut case = make(
                "extended/reserve_spare_fresh",
                65531,
                1,
                false,
                false,
                false,
            );
            case.spare = Some(spare);
            cases.push(case);
        }
    }
    if compressed {
        let mut case = make("extended/takeover_json", 4096, 16, true, true, false);
        case.takeover = true;
        cases.push(case);
    }
    let mut control = make("extended/control_ping", 256, 16, true, false, false);
    control.control = true;
    cases.push(control);
    cases
}
impl Case {
    pub fn verify_reads(&self, reads: &[Read], wire_len: usize) {
        if let Some(spare) = self.spare {
            assert_eq!(
                reads.len(),
                2,
                "no later reserve/read may hide behind the second-read check"
            );
            assert_eq!(reads.iter().map(|read| read.bytes).sum::<usize>(), wire_len);
            assert_eq!(wire_len, sockudo_ws::RECV_BUFFER_SIZE - 1);
            assert_eq!(reads[1].bytes, spare - 1);
            assert_eq!(reads[0].capacity, sockudo_ws::RECV_BUFFER_SIZE);
            assert_eq!(reads[0].bytes, sockudo_ws::RECV_BUFFER_SIZE - spare);
            if spare < 4096 {
                assert!(
                    reads[1].capacity >= 8192,
                    "Compio must reserve before the second read"
                );
            } else {
                assert_eq!(
                    reads[1].capacity, spare,
                    "Compio must use existing spare capacity"
                );
            }
        }
        if self.window {
            assert_eq!(reads[0].bytes, self.size + 4);
        }
        if self.read_limit == 1 {
            assert!(reads.iter().all(|read| read.bytes == 1));
        }
        if self.cut.is_some() {
            assert_eq!(reads.len(), 8);
            for pair in reads.as_chunks::<2>().0 {
                assert_eq!(pair.iter().map(|read| read.bytes).sum::<usize>(), wire_len);
            }
        }
        if self.cut == Some(Cut::Header) {
            assert_eq!(reads[0].bytes, 3);
        }
        if self.cut == Some(Cut::Body) {
            assert!(reads[0].bytes > 8 && reads[0].bytes < self.size);
        }
    }
    pub fn role(&self) -> sockudo_ws::Role {
        if self.server {
            sockudo_ws::Role::Server
        } else {
            sockudo_ws::Role::Client
        }
    }
    pub fn config(&self) -> Config {
        let mut config = if self.timers {
            Config::builder()
                .auto_ping(true)
                .ping_interval(3600)
                .idle_timeout(3600)
                .build()
        } else {
            Config::builder().auto_ping(false).idle_timeout(0).build()
        };
        config.validate_text_utf8 = self.validate_text_utf8;
        config
    }
    pub fn fixture(&self, compressed: bool) -> (Input, Vec<Bytes>) {
        let mut wire = BytesMut::new();
        let mut payloads = Vec::new();
        let json = self
            .text
            .then(|| crate::corpus::json_messages(self.size, self.batch));
        for index in 0..self.batch {
            let mut payload = json
                .as_ref()
                .map_or_else(|| vec![0x42; self.size], |json| json[index].to_vec());
            if self.control && index == 0 {
                payload.truncate(8);
            }
            if !self.text {
                payload[..8].copy_from_slice(format!("{index:08}").as_bytes());
            }
            if self.takeover {
                payloads.push(Bytes::from(payload));
                continue;
            }
            let opcode = if self.control && index == 0 {
                OpCode::Ping
            } else if self.text {
                OpCode::Text
            } else {
                OpCode::Binary
            };
            let mask = self.server.then_some([7, 13, 19, 23]);
            if compressed && opcode != OpCode::Ping {
                #[cfg(feature = "permessage-deflate")]
                {
                    // Independent messages allow a cyclic source without invalid dictionary resets.
                    let mut encoder = sockudo_ws::deflate::DeflateEncoder::new(
                        sockudo_ws::deflate::MAX_WINDOW_BITS,
                        true,
                        6,
                        0,
                    );
                    let encoded = encoder.compress(&payload).unwrap().unwrap();
                    sockudo_ws::frame::encode_frame_with_rsv(
                        &mut wire, opcode, &encoded, true, mask, true,
                    );
                }
                #[cfg(not(feature = "permessage-deflate"))]
                unreachable!("compressed case requires permessage-deflate");
            } else {
                encode_frame(&mut wire, opcode, &payload, true, mask);
            }
            payloads.push(Bytes::from(payload));
        }
        if self.window {
            assert_eq!(wire.len(), self.size + 4);
        }
        let loop_start = if self.takeover {
            #[cfg(feature = "permessage-deflate")]
            {
                let (first, repeated) = crate::corpus::compressed_cycles(&payloads, false);
                let mut loop_start = 0;
                for cycle in [first, repeated] {
                    if !wire.is_empty() {
                        loop_start = wire.len();
                    }
                    for encoded in cycle {
                        sockudo_ws::frame::encode_frame_with_rsv(
                            &mut wire,
                            OpCode::Text,
                            &encoded,
                            true,
                            Some([7, 13, 19, 23]),
                            true,
                        );
                    }
                }
                loop_start
            }
            #[cfg(not(feature = "permessage-deflate"))]
            unreachable!("takeover case requires permessage-deflate")
        } else {
            0
        };
        let mut cuts = Vec::new();
        if let Some(cut) = self.cut {
            let header =
                2 + if wire[1] & 127 == 126 {
                    2
                } else if wire[1] & 127 == 127 {
                    8
                } else {
                    0
                } + if self.server { 4 } else { 0 };
            cuts.push(if cut == Cut::Header {
                3
            } else {
                header + (wire.len() - header) / 2
            });
            assert!(cuts[0] < wire.len());
        }
        if let Some(spare) = self.spare {
            // Fit the complete frame without reserving on the >= 4096-byte side.
            assert_eq!(wire.len(), sockudo_ws::RECV_BUFFER_SIZE - 1);
            cuts.push(sockudo_ws::RECV_BUFFER_SIZE - spare);
        }
        if loop_start > 0 {
            cuts.push(loop_start);
        }
        (
            Input::new(wire.freeze(), self.read_limit, self.pending).with_layout(cuts, loop_start),
            payloads,
        )
    }
}

pub fn retain(held: &mut std::collections::VecDeque<Message>, message: Message, count: usize) {
    if count == 0 {
        std::hint::black_box(message);
    } else {
        held.push_back(message);
        if held.len() > count {
            held.pop_front();
        }
    }
}

// Keep concrete unified and native split types in the timed loop.
macro_rules! receive_case {
    ($c:expr, $runtime:expr, $backend:expr, $kind:expr, $make:expr) => {
        for case in crate::receive_cases::cases($backend, $kind) {
            for first in [false, true] {
                if first && !case.first {
                    continue;
                }
                let metric = if first { "first" } else { "burst" };
                let mut group = $c.benchmark_group(format!(
                    "{}/{}/{}/{metric}",
                    $backend.suite(),
                    case.name,
                    $kind.name()
                ));
                group.throughput(criterion::Throughput::Elements(if first {
                    1
                } else {
                    case.batch as u64
                }));
                let parameter = if let Some(spare) = case.spare {
                    format!("wire{}_spare{spare}", case.size + 4)
                } else if case.window {
                    format!("wire{}", case.size + 4)
                } else {
                    case.size.to_string()
                };
                // Only selected IDs build fixtures; expensive independent proofs run once.
                let fixture = std::cell::OnceCell::new();
                group.bench_function(parameter, |b| {
                    let runtime = $runtime;
                    // Separate trace connection: no capture locks in timed I/O.
                    let (input, payloads) = fixture.get_or_init(|| {
                        let (input, payloads) = case.fixture($kind.compressed());
                        let mut probe = input.clone();
                        let expected = &payloads;
                        let wire_len = probe.wire.len();
                        let reads = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                        probe.reads = Some(reads.clone());
                        runtime.block_on(async {
                            let (mut reader, guard) = ($make)(probe, &case);
                            // The reserve oracle must observe one fresh frame, just like timing.
                            let cycles = if case.spare.is_some() { 1 } else { 4 };
                            for payload in expected.iter().cycle().take(expected.len() * cycles) {
                                assert_eq!(
                                    reader.next().await.unwrap().unwrap().as_bytes(),
                                    payload
                                );
                            }
                            drop((reader, guard));
                        });
                        {
                            let reads = reads.lock().unwrap();
                            case.verify_reads(&reads, wire_len);
                        }
                        (input, payloads)
                    });
                    if case.spare.is_some() {
                        // A warmed buffer no longer has the requested spare capacity.
                        // Time one receive on each fresh stream, excluding its construction/drop.
                        b.iter_custom(|iterations| {
                            runtime.block_on(async {
                                let mut elapsed = std::time::Duration::ZERO;
                                for _ in 0..iterations {
                                    let (mut reader, guard) = ($make)(input.clone(), &case);
                                    let start = std::time::Instant::now();
                                    std::hint::black_box(reader.next().await.unwrap().unwrap());
                                    elapsed += start.elapsed();
                                    drop((reader, guard));
                                }
                                elapsed
                            })
                        });
                        return;
                    }
                    let (mut reader, guard) =
                        runtime.block_on(async { ($make)(input.clone(), &case) });
                    let mut held = std::collections::VecDeque::with_capacity(case.retain + 1);
                    runtime.block_on(async {
                        for (index, payload) in payloads.iter().enumerate() {
                            let message = reader.next().await.unwrap().unwrap();
                            assert_eq!(message.as_bytes(), payload);
                            if case.control && index == 0 {
                                assert!(matches!(message, sockudo_ws::Message::Ping(_)));
                            }
                        }
                    });
                    b.iter_custom(|iterations| {
                        runtime.block_on(async {
                            if first {
                                let mut elapsed = std::time::Duration::ZERO;
                                for _ in 0..iterations {
                                    let start = std::time::Instant::now();
                                    let message = reader.next().await.unwrap().unwrap();
                                    elapsed += start.elapsed();
                                    crate::receive_cases::retain(&mut held, message, case.retain);
                                    for _ in 1..case.batch {
                                        crate::receive_cases::retain(
                                            &mut held,
                                            reader.next().await.unwrap().unwrap(),
                                            case.retain,
                                        );
                                    }
                                }
                                elapsed
                            } else {
                                let start = std::time::Instant::now();
                                for _ in 0..iterations {
                                    for _ in 0..case.batch {
                                        crate::receive_cases::retain(
                                            &mut held,
                                            reader.next().await.unwrap().unwrap(),
                                            case.retain,
                                        );
                                    }
                                }
                                start.elapsed()
                            }
                        })
                    });
                    runtime.block_on(async {
                        for (index, payload) in payloads.iter().enumerate() {
                            let message = reader.next().await.unwrap().unwrap();
                            assert_eq!(message.as_bytes(), payload);
                            if case.control && index == 0 {
                                assert!(matches!(message, sockudo_ws::Message::Ping(_)));
                            }
                        }
                        for message in &held {
                            assert!(
                                payloads
                                    .iter()
                                    .any(|payload| payload.as_ref() == message.as_bytes())
                            );
                        }
                        drop((reader, guard, held));
                    });
                });
                group.finish();
            }
        }
    };
}
pub(crate) use receive_case;
