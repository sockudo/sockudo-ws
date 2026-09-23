use bytes::BytesMut;
use sockudo_ws::error::Error;
use sockudo_ws::frame::{OpCode, encode_frame};
use sockudo_ws::protocol::{Message, Protocol, Role};

#[test]
fn typed_partial_continuation_resumes_after_raw_and_control_frames() {
    for mask in [None, Some([1, 2, 3, 4])] {
        let role = if mask.is_some() {
            Role::Server
        } else {
            Role::Client
        };
        let mut protocol = Protocol::new(role, 1024, 1024);
        let mut buf = BytesMut::new();
        encode_frame(&mut buf, OpCode::Text, b"a\xe7", false, mask);
        assert!(protocol.process(&mut buf).unwrap().is_empty());
        encode_frame(&mut buf, OpCode::Continuation, b"\x95\x8c\xf0", false, mask);
        assert!(protocol.process_raw(&mut buf).unwrap().is_empty());
        encode_frame(&mut buf, OpCode::Ping, b"ping", true, mask);
        assert!(protocol.process(&mut buf).unwrap()[0].is_ping());

        let mut tail = BytesMut::new();
        encode_frame(
            &mut tail,
            OpCode::Continuation,
            b"\x9f\xa6\x80z",
            true,
            mask,
        );
        let mut messages = Vec::new();
        for byte in tail {
            buf.extend_from_slice(&[byte]);
            messages.extend(protocol.process(&mut buf).unwrap());
        }
        assert!(
            matches!(&messages[..], [Message::Text(text)] if text.as_ref() == "a界🦀z".as_bytes())
        );
    }
}

#[test]
fn invalid_text_prefix_fails_before_frame_completes() {
    for mask in [None, Some([1, 2, 3, 4])] {
        let role = if mask.is_some() {
            Role::Server
        } else {
            Role::Client
        };
        let mut protocol = Protocol::new(role, 1024, 1024);
        let mut wire = BytesMut::new();
        encode_frame(&mut wire, OpCode::Text, b"\xf4\x90tail", true, mask);
        wire.truncate(wire.len() - 4);
        assert!(matches!(
            protocol.process(&mut wire),
            Err(Error::InvalidUtf8)
        ));
    }
}

#[test]
fn text_split_across_reads_and_frames_preserves_codepoints() {
    for mask in [None, Some([1, 2, 3, 4])] {
        let role = if mask.is_some() {
            Role::Server
        } else {
            Role::Client
        };
        let mut protocol = Protocol::new(role, 1024, 1024);
        let mut wire = BytesMut::new();
        encode_frame(&mut wire, OpCode::Text, b"a\xf0", false, mask);
        encode_frame(&mut wire, OpCode::Ping, b"ping", true, mask);
        encode_frame(
            &mut wire,
            OpCode::Continuation,
            b"\x9f\x8e\x89z",
            true,
            mask,
        );
        let mut buf = BytesMut::new();
        let mut messages = Vec::new();
        for byte in wire {
            buf.extend_from_slice(&[byte]);
            messages.extend(protocol.process(&mut buf).unwrap());
        }
        assert!(
            matches!(&messages[..], [Message::Ping(p), Message::Text(t)] if p == "ping" && t == "a🎉z")
        );
    }
}

#[test]
fn raw_fragment_is_validated_when_typed_partial_continuation_arrives() {
    let mut protocol = Protocol::new(Role::Client, 1024, 1024);
    let mut buf = BytesMut::new();
    encode_frame(&mut buf, OpCode::Text, b"\xff", false, None);
    protocol.process_raw(&mut buf).unwrap();
    encode_frame(&mut buf, OpCode::Continuation, b"tail", true, None);
    buf.truncate(buf.len() - 2);
    assert!(matches!(
        protocol.process(&mut buf),
        Err(Error::InvalidUtf8)
    ));
}

#[test]
fn typed_raw_typed_switch_preserves_unvalidated_fragment_suffix() {
    let mut protocol = Protocol::new(Role::Client, 1024, 1024);
    let mut buf = BytesMut::new();
    encode_frame(&mut buf, OpCode::Text, b"a", false, None);
    protocol.process(&mut buf).unwrap();
    encode_frame(&mut buf, OpCode::Continuation, b"\xff", false, None);
    protocol.process_raw(&mut buf).unwrap();
    encode_frame(&mut buf, OpCode::Continuation, b"z", true, None);
    assert!(matches!(
        protocol.process(&mut buf),
        Err(Error::InvalidUtf8)
    ));
}

#[test]
fn incremental_masking_matches_whole_text_for_chunk_boundaries() {
    let payload = "Hello, 世界! 🎉 κόσμε éà ".repeat(16);
    for chunk_len in 1..=65 {
        let mut wire = BytesMut::new();
        encode_frame(
            &mut wire,
            OpCode::Text,
            payload.as_bytes(),
            true,
            Some([7, 13, 19, 23]),
        );
        let mut protocol = Protocol::new(Role::Server, 4096, 4096);
        let mut buf = BytesMut::new();
        let mut messages = Vec::new();
        for chunk in wire.chunks(chunk_len) {
            buf.extend_from_slice(chunk);
            messages.extend(protocol.process(&mut buf).unwrap());
        }
        assert!(matches!(&messages[..], [Message::Text(t)] if t.as_ref() == payload.as_bytes()));
    }
}

#[test]
fn raw_completion_clears_partial_text_validation_before_next_message() {
    let mut protocol = Protocol::new(Role::Client, 1024, 1024);
    let mut wire = BytesMut::new();
    encode_frame(&mut wire, OpCode::Text, b"abcd", true, None);
    let mut buf = wire.split_to(wire.len() - 2);
    assert!(protocol.process(&mut buf).unwrap().is_empty());
    buf.extend_from_slice(&wire);
    assert_eq!(protocol.process_raw(&mut buf).unwrap().len(), 1);
    encode_frame(&mut buf, OpCode::Text, b"\xff", true, None);
    assert!(matches!(
        protocol.process(&mut buf),
        Err(Error::InvalidUtf8)
    ));
}
