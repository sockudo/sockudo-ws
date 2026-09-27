use bytes::BytesMut;
use sockudo_ws::Error;
use sockudo_ws::frame::{FrameParser, OpCode, encode_frame, encode_frame_with_rsv};

#[test]
fn frame_limit_boundaries_do_not_depend_on_read_segmentation() {
    for limit in [0, 1, 124, 125, 126, 65535, 65536] {
        for size in [limit, limit + 1] {
            for mask in [None, Some([1, 2, 3, 4])] {
                let mut wire = BytesMut::new();
                encode_frame(&mut wire, OpCode::Binary, &vec![b'x'; size], true, mask);
                for split in [0, 1, wire.len() - 1, wire.len()] {
                    let mut parser = FrameParser::new(limit, mask.is_some());
                    let mut input = BytesMut::from(&wire[..split]);
                    let result = match parser.parse(&mut input) {
                        Ok(None) => {
                            input.extend_from_slice(&wire[split..]);
                            parser.parse(&mut input)
                        }
                        result => result,
                    };
                    if size == limit {
                        let frame = result.unwrap().unwrap();
                        assert_eq!(frame.payload.as_ref(), vec![b'x'; size]);
                        assert!(input.is_empty());
                    } else {
                        assert!(matches!(result, Err(Error::FrameTooLarge)));
                    }
                }
            }
        }
    }
}

#[test]
fn small_frame_limits_apply_to_complete_masked_and_unmasked_frames() {
    let rejected = [None, Some([0x12, 0x34, 0x56, 0x78])].map(|mask| {
        let mut parser = FrameParser::new(124, mask.is_some());
        let mut wire = BytesMut::new();
        encode_frame(&mut wire, OpCode::Binary, &[b'x'; 125], true, mask);
        matches!(parser.parse(&mut wire), Err(Error::FrameTooLarge))
    });
    assert_eq!(rejected, [true, true]);
}

#[test]
fn unmasked_data_frames_survive_header_and_payload_splits() {
    for size in [125, 126, 193, 65535, 65536] {
        for (opcode, fin, compressed) in [
            (OpCode::Text, true, false),
            (OpCode::Binary, false, true),
            (OpCode::Continuation, true, false),
        ] {
            let payload = vec![b'x'; size];
            let mut wire = BytesMut::new();
            encode_frame_with_rsv(&mut wire, opcode, &payload, fin, None, compressed);
            for split in [
                0,
                1,
                2,
                3,
                4,
                9,
                10,
                wire.len() / 2,
                wire.len() - 1,
                wire.len(),
            ] {
                let mut parser = FrameParser::new(size, false);
                parser.set_compression(compressed);
                let mut input = BytesMut::from(&wire[..split]);
                let first = parser.parse(&mut input).unwrap();
                input.extend_from_slice(&wire[split..]);
                let frame = first.or_else(|| parser.parse(&mut input).unwrap()).unwrap();
                assert_eq!(frame.payload.as_ref(), payload);
                assert_eq!(frame.header.opcode, opcode);
                assert_eq!(frame.header.fin, fin);
                assert_eq!(frame.header.rsv1, compressed);
                assert!(!frame.header.rsv2 && !frame.header.rsv3 && !frame.header.masked);
                assert_eq!(frame.header.mask, None);
                assert_eq!(frame.header.payload_len, size as u64);
                assert!(input.is_empty());
            }
        }
    }
}

#[test]
fn coalesced_medium_frames_preserve_following_control_and_data_frames() {
    for size in [126, 193, 65535] {
        let mut parser = FrameParser::new(size, false);
        let payload = vec![b'x'; size];
        let mut wire = BytesMut::new();
        encode_frame(&mut wire, OpCode::Binary, &payload, true, None);
        encode_frame(&mut wire, OpCode::Ping, b"ping", true, None);
        encode_frame(&mut wire, OpCode::Text, b"next", true, None);
        let frames: Vec<_> = (0..3)
            .map(|_| parser.parse(&mut wire).unwrap().unwrap())
            .collect();
        assert_eq!(frames[0].payload.as_ref(), payload);
        assert_eq!(frames[1].header.opcode, OpCode::Ping);
        assert_eq!(frames[1].payload.as_ref(), b"ping");
        assert_eq!(frames[2].payload.as_ref(), b"next");
        assert!(wire.is_empty());
    }
}

#[test]
fn medium_frame_limits_apply_to_headers_and_complete_frames() {
    for size in [126_u16, 193, 65535] {
        for split in [0, 2] {
            for payload_present in [false, true] {
                let mut parser = FrameParser::new(usize::from(size) - 1, false);
                let length = size.to_be_bytes();
                let header = [0x82, 126, length[0], length[1]];
                let mut input = BytesMut::from(&header[..split]);
                assert!(parser.parse(&mut input).unwrap().is_none());
                input.extend_from_slice(&header[split..]);
                if payload_present {
                    input.extend_from_slice(&vec![b'x'; usize::from(size)]);
                }
                assert!(matches!(
                    parser.parse(&mut input),
                    Err(Error::FrameTooLarge)
                ));
            }
        }
    }
}

#[test]
fn medium_frames_preserve_protocol_errors_across_read_boundaries() {
    for (header, expected) in [
        (
            [0x81, 126, 0, 125],
            Error::Protocol("payload length not minimal"),
        ),
        (
            [0x89, 126, 0, 126],
            Error::Protocol("control frame too large"),
        ),
        (
            [0x09, 126, 0, 126],
            Error::Protocol("control frame must not be fragmented"),
        ),
        (
            [0xc1, 126, 0, 126],
            Error::Protocol("RSV1 must be 0 (compression not negotiated)"),
        ),
        (
            [0xa1, 126, 0, 126],
            Error::Protocol("RSV2 and RSV3 must be 0"),
        ),
        (
            [0x91, 126, 0, 126],
            Error::Protocol("RSV2 and RSV3 must be 0"),
        ),
        ([0x83, 126, 0, 126], Error::InvalidFrame("invalid opcode")),
        (
            [0x81, 254, 0, 126],
            Error::Protocol("server frames must not be masked"),
        ),
    ] {
        let mut wire = BytesMut::from(header.as_slice());
        wire.extend_from_slice(&[b'x'; 126]);
        for split in [0, 1, 2, 3, 4, wire.len()] {
            let mut parser = FrameParser::new(65536, false);
            let mut input = BytesMut::from(&wire[..split]);
            let result = match parser.parse(&mut input) {
                Ok(None) => {
                    input.extend_from_slice(&wire[split..]);
                    parser.parse(&mut input)
                }
                result => result,
            };
            assert_eq!(result.unwrap_err().to_string(), expected.to_string());
        }
    }
}

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

#[test]
fn medium_frame_header_errors_precede_size_errors() {
    for masked in [false, true] {
        for (first, expected) in [
            (0xa2, "Protocol error: RSV2 and RSV3 must be 0"),
            (0x83, "Invalid frame: invalid opcode"),
            (0x09, "Protocol error: control frame must not be fragmented"),
        ] {
            let mut wire =
                BytesMut::from(&[first, 126 | if masked { 128 } else { 0 }, 1, 0, 0, 0, 0, 0][..]);
            let mut parser = FrameParser::new(128, masked);
            assert_eq!(parser.parse(&mut wire).unwrap_err().to_string(), expected);
        }
    }
}
