//! Protocol text validation, including raw/typed switching and partial reads.
//! Input encoding, chunk copies and Protocol construction are outside timing.
//! Timing includes appending chunks to the receive buffer, parsing, unmasking,
//! fragment assembly, validation, output Vec allocation and input/Protocol drops.
//! Returned messages are dropped outside the measured routine.

use bytes::{Bytes, BytesMut};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use sockudo_ws::frame::{OpCode, encode_frame};
use sockudo_ws::protocol::{Message, Protocol, RawMessage, Role};
use std::hint::black_box;

struct Input {
    chunks: Vec<(bool, Bytes)>,
    payload: Vec<u8>,
    role: Role,
}

impl Input {
    fn new(
        size: usize,
        unicode: bool,
        fragments: usize,
        mixed: bool,
        partial: bool,
        masked: bool,
    ) -> Self {
        let payload = if unicode {
            "abc界é".repeat(size / 8).into_bytes()
        } else {
            vec![b'a'; size]
        };
        let mut chunks = Vec::new();
        for index in 0..fragments {
            // Offset internal boundaries into multibyte code points as well as ASCII.
            let start = if index == 0 {
                0
            } else {
                index * size / fragments + 4
            };
            let end = if index + 1 == fragments {
                size
            } else {
                (index + 1) * size / fragments + 4
            };
            let opcode = if index == 0 {
                OpCode::Text
            } else {
                OpCode::Continuation
            };
            let mut frame = BytesMut::new();
            encode_frame(
                &mut frame,
                opcode,
                &payload[start..end],
                index + 1 == fragments,
                masked.then_some([7, 13, 19, 23]),
            );
            // Finish typed, so mixed cases must validate all prior raw fragments.
            let raw = mixed && index % 2 == 0;
            for chunk in frame.chunks(if partial { 31 } else { frame.len() }) {
                chunks.push((raw, Bytes::copy_from_slice(chunk)));
            }
        }
        Self {
            chunks,
            payload,
            role: if masked { Role::Server } else { Role::Client },
        }
    }

    fn setup(&self) -> (Protocol, BytesMut, Vec<(bool, BytesMut)>) {
        (
            Protocol::new(self.role, 1 << 20, 1 << 20),
            BytesMut::with_capacity(self.payload.len() + 14),
            self.chunks
                .iter()
                .map(|(raw, bytes)| (*raw, BytesMut::from(bytes.as_ref())))
                .collect(),
        )
    }

    fn verify(&self) {
        assert!(std::str::from_utf8(&self.payload).is_ok());
        let messages = process(self.setup());
        assert!(matches!(&messages[..], [Message::Text(bytes)] if bytes.as_ref() == self.payload));
    }
}

fn process(
    (mut protocol, mut buffer, chunks): (Protocol, BytesMut, Vec<(bool, BytesMut)>),
) -> Vec<Message> {
    let mut messages = Vec::new();
    let mut raw_messages: Vec<RawMessage> = Vec::new();
    for (raw, chunk) in chunks {
        buffer.extend_from_slice(black_box(&chunk));
        if raw {
            protocol
                .process_raw_into(&mut buffer, &mut raw_messages)
                .unwrap();
            assert!(raw_messages.is_empty());
        } else {
            protocol.process_into(&mut buffer, &mut messages).unwrap();
        }
    }
    assert!(buffer.is_empty());
    black_box(messages)
}

fn bench_fragments(c: &mut Criterion) {
    let mut group = c.benchmark_group("fragment_validation");
    for masked in [false, true] {
        let direction = if masked { "masked" } else { "unmasked" };
        for (size, unicode, fragments, mixed, partial) in [
            (64, false, 1, false, false),
            (64, true, 1, false, false),
            (4096, false, 1, false, false),
            (4096, true, 1, false, false),
            (4096, false, 4, false, false),
            (4096, true, 4, false, false),
            (4096, false, 64, false, false),
            (4096, true, 64, false, false),
            (4096, false, 4, true, false),
            (4096, false, 64, true, false),
            (4096, false, 4, false, true),
            (4096, false, 4, true, true),
        ] {
            let input = Input::new(size, unicode, fragments, mixed, partial, masked);
            let text = if unicode { "utf8" } else { "ascii" };
            let mode = if mixed { "mixed" } else { "typed" };
            let reads = if partial { "partial31" } else { "whole" };
            let name = format!("{direction}/{text}_{size}_f{fragments}_{mode}_{reads}");
            input.verify();
            group.bench_function(name, |b| {
                b.iter_batched(|| input.setup(), process, BatchSize::SmallInput);
            });
            input.verify();
        }
    }
    group.finish();
}

criterion_group!(benches, bench_fragments);
criterion_main!(benches);
