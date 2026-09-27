//! Steady-state parsing and single-frame delivery with finite size limits.
//! Encoding, wire-buffer copies, protocol construction and output destruction
//! are outside timing. Parsing, unmasking, text validation, message allocation
//! and destruction of the consumed input buffer are inside timing.

use bytes::BytesMut;
use criterion::measurement::WallTime;
use criterion::{BatchSize, BenchmarkGroup, Criterion, criterion_group, criterion_main};
use sockudo_ws::deflate::{DeflateConfig, DeflateEncoder, MAX_WINDOW_BITS};
use sockudo_ws::frame::{FrameParser, OpCode, encode_frame, encode_frame_with_rsv};
use sockudo_ws::protocol::{
    CompressedProtocol, CompressedReaderProtocol, Message, Protocol, RawMessage, Role,
};
use std::hint::black_box;

fn measure<T>(
    group: &mut BenchmarkGroup<'_, WallTime>,
    name: String,
    wire: &[u8],
    mut parse: impl FnMut(&mut BytesMut) -> T,
    verify: impl Fn(T),
) {
    let mut input = BytesMut::from(wire);
    verify(parse(&mut input));
    assert!(input.is_empty());
    group.bench_function(name, |b| {
        b.iter_batched(
            || BytesMut::from(wire),
            |mut input| black_box(parse(black_box(&mut input))),
            BatchSize::SmallInput,
        );
    });
    let mut input = BytesMut::from(wire);
    verify(parse(&mut input));
    assert!(input.is_empty());
}

fn verify_messages(messages: Vec<Message>, opcode: OpCode, payload: &[u8]) {
    assert_eq!(messages.len(), 1);
    assert!(matches!(
        (&messages[0], opcode),
        (Message::Text(_), OpCode::Text) | (Message::Binary(_), OpCode::Binary)
    ));
    assert_eq!(messages[0].as_bytes(), payload);
}

fn bench_receive_limits(c: &mut Criterion) {
    let mut group = c.benchmark_group("receive_limits");
    for masked in [false, true] {
        let direction = if masked { "masked" } else { "unmasked" };
        let role = if masked { Role::Server } else { Role::Client };
        let mask = masked.then_some([7, 13, 19, 23]);
        for size in [64, 125, 126, 4096] {
            let payload = vec![b'x'; size];
            let mut wire = BytesMut::new();
            encode_frame(&mut wire, OpCode::Binary, &payload, true, mask);
            let mut parser = FrameParser::new(1 << 20, masked);
            measure(
                &mut group,
                format!("{direction}_frame_{size}"),
                &wire,
                |input| parser.parse(input).unwrap().unwrap(),
                |frame| {
                    assert_eq!(frame.header.opcode, OpCode::Binary);
                    assert!(frame.header.fin);
                    assert_eq!(frame.header.masked, masked);
                    assert_eq!(frame.payload.as_ref(), payload);
                },
            );
        }
        for size in [64, 126] {
            for opcode in [OpCode::Text, OpCode::Binary] {
                let kind = if opcode == OpCode::Text {
                    "text"
                } else {
                    "binary"
                };
                let payload = vec![b'x'; size];
                assert!(std::str::from_utf8(&payload).is_ok());
                let mut wire = BytesMut::new();
                encode_frame(&mut wire, opcode, &payload, true, mask);
                let mut typed = Protocol::new(role, 1 << 20, 1 << 20);
                measure(
                    &mut group,
                    format!("{direction}_typed_{kind}_{size}"),
                    &wire,
                    |input| typed.process(input).unwrap(),
                    |messages| verify_messages(messages, opcode, &payload),
                );
                let mut raw = Protocol::new(role, 1 << 20, 1 << 20);
                measure(
                    &mut group,
                    format!("{direction}_raw_{kind}_{size}"),
                    &wire,
                    |input| raw.process_raw(input).unwrap(),
                    |messages| {
                        assert_eq!(messages.len(), 1);
                        assert!(matches!(
                            (&messages[0], opcode),
                            (RawMessage::Text(_), OpCode::Text)
                                | (RawMessage::Binary(_), OpCode::Binary)
                        ));
                        assert_eq!(messages[0].as_bytes(), payload);
                    },
                );
                // RSV1 stays clear: these cases reach the uncompressed-input
                // branches of compression-capable protocols, not the codec.
                let config = DeflateConfig::default();
                let mut compressed = if masked {
                    CompressedProtocol::server(1 << 20, 1 << 20, config.clone())
                } else {
                    CompressedProtocol::client(1 << 20, 1 << 20, config.clone())
                };
                measure(
                    &mut group,
                    format!("{direction}_compressed_{kind}_{size}"),
                    &wire,
                    |input| compressed.process(input).unwrap(),
                    |messages| verify_messages(messages, opcode, &payload),
                );
                let mut reader = if masked {
                    CompressedReaderProtocol::server(1 << 20, 1 << 20, &config)
                } else {
                    CompressedReaderProtocol::client(1 << 20, 1 << 20, &config)
                };
                measure(
                    &mut group,
                    format!("{direction}_reader_{kind}_{size}"),
                    &wire,
                    |input| reader.process(input).unwrap(),
                    |messages| verify_messages(messages, opcode, &payload),
                );
            }
        }

        for size in [64, 4096] {
            let payload = vec![b'x'; size];
            let mut encoder = DeflateEncoder::new(MAX_WINDOW_BITS, true, 6, 0);
            let compressed = encoder.compress(&payload).unwrap().unwrap();
            let mut wire = BytesMut::new();
            encode_frame_with_rsv(&mut wire, OpCode::Binary, &compressed, true, mask, true);
            let config = DeflateConfig::default();
            let mut unified = if masked {
                CompressedProtocol::server(1 << 20, 1 << 20, config.clone())
            } else {
                CompressedProtocol::client(1 << 20, 1 << 20, config.clone())
            };
            measure(
                &mut group,
                format!("{direction}_compressed_rsv1_binary_{size}"),
                &wire,
                |input| unified.process(input).unwrap(),
                |messages| verify_messages(messages, OpCode::Binary, &payload),
            );
            let mut reader = if masked {
                CompressedReaderProtocol::server(1 << 20, 1 << 20, &config)
            } else {
                CompressedReaderProtocol::client(1 << 20, 1 << 20, &config)
            };
            measure(
                &mut group,
                format!("{direction}_reader_rsv1_binary_{size}"),
                &wire,
                |input| reader.process(input).unwrap(),
                |messages| verify_messages(messages, OpCode::Binary, &payload),
            );
        }

        let mut wire = BytesMut::new();
        encode_frame(&mut wire, OpCode::Ping, b"ping", true, mask);
        let config = DeflateConfig::default();
        let mut unified = if masked {
            CompressedProtocol::server(1 << 20, 1 << 20, config.clone())
        } else {
            CompressedProtocol::client(1 << 20, 1 << 20, config.clone())
        };
        measure(
            &mut group,
            format!("{direction}_compressed_ping"),
            &wire,
            |input| unified.process(input).unwrap(),
            |messages| {
                assert_eq!(messages.len(), 1);
                assert!(matches!(&messages[0], Message::Ping(bytes) if bytes.as_ref() == b"ping"));
            },
        );
        let mut reader = if masked {
            CompressedReaderProtocol::server(1 << 20, 1 << 20, &config)
        } else {
            CompressedReaderProtocol::client(1 << 20, 1 << 20, &config)
        };
        measure(
            &mut group,
            format!("{direction}_reader_ping"),
            &wire,
            |input| reader.process(input).unwrap(),
            |messages| {
                assert_eq!(messages.len(), 1);
                assert!(matches!(&messages[0], Message::Ping(bytes) if bytes.as_ref() == b"ping"));
            },
        );

        let payload = b"fragmented compressed payload ".repeat(8);
        let mut encoder = DeflateEncoder::new(MAX_WINDOW_BITS, true, 6, 0);
        let compressed = encoder.compress(&payload).unwrap().unwrap();
        let midpoint = compressed.len() / 2;
        assert!(midpoint > 0);
        let mut wire = BytesMut::new();
        encode_frame_with_rsv(
            &mut wire,
            OpCode::Binary,
            &compressed[..midpoint],
            false,
            mask,
            true,
        );
        encode_frame(&mut wire, OpCode::Ping, b"ping", true, mask);
        encode_frame(
            &mut wire,
            OpCode::Continuation,
            &compressed[midpoint..],
            true,
            mask,
        );
        let config = DeflateConfig::default();
        let mut unified = if masked {
            CompressedProtocol::server(1 << 20, 1 << 20, config.clone())
        } else {
            CompressedProtocol::client(1 << 20, 1 << 20, config.clone())
        };
        measure(
            &mut group,
            format!("{direction}_compressed_fragmented_ping"),
            &wire,
            |input| unified.process(input).unwrap(),
            |messages| {
                assert_eq!(messages.len(), 2);
                assert!(matches!(&messages[0], Message::Ping(bytes) if bytes.as_ref() == b"ping"));
                assert!(
                    matches!(&messages[1], Message::Binary(bytes) if bytes.as_ref() == payload)
                );
            },
        );
        let mut reader = if masked {
            CompressedReaderProtocol::server(1 << 20, 1 << 20, &config)
        } else {
            CompressedReaderProtocol::client(1 << 20, 1 << 20, &config)
        };
        measure(
            &mut group,
            format!("{direction}_reader_fragmented_ping"),
            &wire,
            |input| reader.process(input).unwrap(),
            |messages| {
                assert_eq!(messages.len(), 2);
                assert!(matches!(&messages[0], Message::Ping(bytes) if bytes.as_ref() == b"ping"));
                assert!(
                    matches!(&messages[1], Message::Binary(bytes) if bytes.as_ref() == payload)
                );
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_receive_limits);
criterion_main!(benches);
