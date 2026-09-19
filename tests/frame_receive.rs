use bytes::BytesMut;
use sockudo_ws::Error;
use sockudo_ws::frame::{FrameParser, OpCode, encode_frame, encode_frame_with_rsv};

#[test]
fn masked_data_frames_survive_header_and_payload_splits() {
    let mask = [0x12, 0x34, 0x56, 0x78];
    for size in [125, 126, 256, 65535, 65536] {
        for (opcode, fin, compressed) in [
            (OpCode::Text, true, false),
            (OpCode::Binary, false, true),
            (OpCode::Continuation, true, false),
        ] {
            let payload: Vec<_> = (0..size).map(|i| (i % 251) as u8).collect();
            let mut wire = BytesMut::new();
            encode_frame_with_rsv(&mut wire, opcode, &payload, fin, Some(mask), compressed);
            for split in [
                0,
                1,
                2,
                3,
                4,
                5,
                6,
                7,
                8,
                13,
                14,
                wire.len() / 2,
                wire.len() - 1,
                wire.len(),
            ] {
                let mut parser = FrameParser::new(size, true);
                parser.set_compression(compressed);
                let mut input = BytesMut::from(&wire[..split]);
                let first = parser.parse(&mut input).unwrap();
                input.extend_from_slice(&wire[split..]);
                let frame = first.or_else(|| parser.parse(&mut input).unwrap()).unwrap();
                assert_eq!(frame.payload.as_ref(), payload);
                assert_eq!(frame.header.opcode, opcode);
                assert_eq!(frame.header.fin, fin);
                assert_eq!(frame.header.rsv1, compressed);
                assert!(frame.header.masked && !frame.header.rsv2 && !frame.header.rsv3);
                assert_eq!(frame.header.mask, Some(mask));
                assert_eq!(frame.header.payload_len, size as u64);
                assert!(input.is_empty());
            }
        }
    }
}

#[test]
fn masked_medium_limits_apply_before_payload_arrives() {
    for size in [126_u16, 256, 65535] {
        for available in [4, 8, usize::from(size) + 8] {
            let mut wire = BytesMut::new();
            encode_frame(
                &mut wire,
                OpCode::Binary,
                &vec![b'x'; usize::from(size)],
                true,
                Some([1, 2, 3, 4]),
            );
            wire.truncate(available);
            let mut parser = FrameParser::new(usize::from(size) - 1, true);
            assert!(matches!(parser.parse(&mut wire), Err(Error::FrameTooLarge)));
        }
    }
}

#[test]
fn masked_medium_frames_preserve_protocol_errors() {
    for (header, expected) in [
        (
            [0x82, 254, 0, 125],
            "Protocol error: payload length not minimal",
        ),
        (
            [0x89, 254, 0, 126],
            "Protocol error: control frame too large",
        ),
        (
            [0x09, 254, 0, 126],
            "Protocol error: control frame must not be fragmented",
        ),
        (
            [0xc2, 254, 0, 126],
            "Protocol error: RSV1 must be 0 (compression not negotiated)",
        ),
        (
            [0xa2, 254, 0, 126],
            "Protocol error: RSV2 and RSV3 must be 0",
        ),
        (
            [0x92, 254, 0, 126],
            "Protocol error: RSV2 and RSV3 must be 0",
        ),
        ([0x83, 254, 0, 126], "Invalid frame: invalid opcode"),
        (
            [0x82, 126, 0, 126],
            "Protocol error: client frames must be masked",
        ),
    ] {
        let mut wire = BytesMut::from(header.as_slice());
        wire.extend_from_slice(&[1, 2, 3, 4]);
        wire.extend_from_slice(&[b'x'; 126]);
        for split in [0, 1, 2, 3, 4, 6, 7, 8, wire.len()] {
            let mut parser = FrameParser::new(65536, true);
            let mut input = BytesMut::from(&wire[..split]);
            let result = match parser.parse(&mut input) {
                Ok(None) => {
                    input.extend_from_slice(&wire[split..]);
                    parser.parse(&mut input)
                }
                result => result,
            };
            assert_eq!(result.unwrap_err().to_string(), expected);
        }
    }
}
