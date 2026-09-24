use bytes::BytesMut;
use sockudo_ws::Error;
use sockudo_ws::frame::{OpCode, encode_frame};
use sockudo_ws::protocol::{Protocol, Role};
use sockudo_ws::utf8::validate_utf8_incomplete;

#[test]
fn raw_message_reuses_matching_carry_from_previous_message() {
    for chunk_size in [1, usize::MAX] {
        let mut protocol = Protocol::new(Role::Client, 1024, 1024);
        let mut wire = BytesMut::new();
        encode_frame(&mut wire, OpCode::Text, b"a\xe2", false, None);
        assert!(protocol.process(&mut wire).unwrap().is_empty());
        encode_frame(&mut wire, OpCode::Continuation, b"\x82\xac", true, None);
        assert_eq!(protocol.process_raw(&mut wire).unwrap().len(), 1);
        encode_frame(&mut wire, OpCode::Text, b"\xe2", false, None);
        assert!(protocol.process_raw(&mut wire).unwrap().is_empty());

        let mut tail = BytesMut::new();
        encode_frame(&mut tail, OpCode::Continuation, b"\x82\xac", true, None);
        let mut messages = Vec::new();
        for chunk in tail.chunks(chunk_size) {
            wire.extend_from_slice(chunk);
            messages.extend(protocol.process(&mut wire).unwrap());
        }
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].as_bytes(), "€".as_bytes());
    }
}

#[test]
fn raw_message_does_not_reuse_different_carry_of_same_length() {
    for chunk_size in [1, usize::MAX] {
        let mut protocol = Protocol::new(Role::Client, 1024, 1024);
        let mut wire = BytesMut::new();
        encode_frame(&mut wire, OpCode::Text, b"a\xe2", false, None);
        assert!(protocol.process(&mut wire).unwrap().is_empty());
        encode_frame(&mut wire, OpCode::Continuation, b"\x82\xac", true, None);
        assert_eq!(protocol.process_raw(&mut wire).unwrap().len(), 1);
        encode_frame(&mut wire, OpCode::Text, b"\xff", false, None);
        assert!(protocol.process_raw(&mut wire).unwrap().is_empty());

        let mut tail = BytesMut::new();
        encode_frame(&mut tail, OpCode::Continuation, b"\x82\xac", true, None);
        let result = tail.chunks(chunk_size).try_for_each(|chunk| {
            wire.extend_from_slice(chunk);
            protocol.process(&mut wire).map(|_| ())
        });
        assert!(matches!(result, Err(Error::InvalidUtf8)));
    }
}

#[test]
fn partial_utf8_validation_preserves_suffixes_after_ascii_prefixes() {
    for length in [0, 1, 15, 16, 31, 32, 63, 64, 65, 4096] {
        for (suffix, expected) in [
            (b"".as_slice(), (true, 0)),
            (b"\xe7", (true, 1)),
            (b"\xe7\x95", (true, 2)),
            (b"\xe7\x95\x8c", (true, 0)),
            (b"\xff", (false, 0)),
            (b"\xed\xa0", (false, 0)),
        ] {
            let mut payload = vec![b'a'; length];
            payload.extend_from_slice(suffix);
            assert_eq!(validate_utf8_incomplete(&payload), expected);
        }
    }
}

#[test]
fn fragmented_text_matches_utf8_oracle_at_every_split() {
    for payload in [
        "a界🦀z".as_bytes(),
        b"prefix\xf0\x9f\xa6",
        b"prefix\xed\xa0\x80",
        b"prefix\xc0\x80",
        b"prefix\xf4\x90\x80\x80",
        b"prefix\x80suffix",
    ] {
        for first in 0..=payload.len() {
            for second in first..=payload.len() {
                let mut wire = BytesMut::new();
                encode_frame(&mut wire, OpCode::Text, &payload[..first], false, None);
                encode_frame(
                    &mut wire,
                    OpCode::Continuation,
                    &payload[first..second],
                    false,
                    None,
                );
                encode_frame(
                    &mut wire,
                    OpCode::Continuation,
                    &payload[second..],
                    true,
                    None,
                );
                let result = Protocol::new(Role::Client, 1024, 1024).process(&mut wire);
                if std::str::from_utf8(payload).is_ok() {
                    let messages = result.unwrap();
                    assert_eq!(messages.len(), 1);
                    assert_eq!(messages[0].as_bytes(), payload);
                } else {
                    assert!(matches!(result, Err(Error::InvalidUtf8)));
                }
            }
        }
    }
}

#[test]
fn control_frames_do_not_interrupt_multibyte_fragments() {
    let mut wire = BytesMut::new();
    encode_frame(&mut wire, OpCode::Text, b"valid\xf0", false, None);
    encode_frame(&mut wire, OpCode::Continuation, b"\x9f", false, None);
    encode_frame(&mut wire, OpCode::Ping, b"ping", true, None);
    encode_frame(&mut wire, OpCode::Continuation, b"\xa6", false, None);
    encode_frame(&mut wire, OpCode::Pong, b"pong", true, None);
    encode_frame(&mut wire, OpCode::Continuation, b"\x80", true, None);
    let messages = Protocol::new(Role::Client, 1024, 1024)
        .process(&mut wire)
        .unwrap();
    assert_eq!(messages.len(), 3);
    assert!(messages[0].is_ping());
    assert!(messages[1].is_pong());
    assert_eq!(messages[2].as_bytes(), "valid🦀".as_bytes());
}

#[test]
fn invalid_continuation_is_rejected_before_final_fragment() {
    let mut wire = BytesMut::new();
    encode_frame(&mut wire, OpCode::Text, b"valid\xe7", false, None);
    encode_frame(&mut wire, OpCode::Continuation, b"x", false, None);
    assert!(matches!(
        Protocol::new(Role::Client, 1024, 1024).process(&mut wire),
        Err(Error::InvalidUtf8)
    ));
}

#[test]
fn switching_from_raw_processing_still_validates_text() {
    let mut protocol = Protocol::new(Role::Client, 1024, 1024);
    let mut wire = BytesMut::new();
    encode_frame(&mut wire, OpCode::Text, b"\xff", false, None);
    assert!(protocol.process_raw(&mut wire).unwrap().is_empty());
    encode_frame(&mut wire, OpCode::Continuation, b"ok", true, None);
    assert!(matches!(
        protocol.process(&mut wire),
        Err(Error::InvalidUtf8)
    ));
}

#[test]
fn typed_processing_validates_bytes_appended_by_raw_continuations() {
    let mut protocol = Protocol::new(Role::Client, 1024, 1024);
    let mut wire = BytesMut::new();
    encode_frame(&mut wire, OpCode::Text, b"valid\xe7", false, None);
    assert!(protocol.process(&mut wire).unwrap().is_empty());
    encode_frame(
        &mut wire,
        OpCode::Continuation,
        b"\x95\x8c\xff",
        false,
        None,
    );
    assert!(protocol.process_raw(&mut wire).unwrap().is_empty());
    encode_frame(&mut wire, OpCode::Continuation, b"tail", false, None);
    assert!(matches!(
        protocol.process(&mut wire),
        Err(Error::InvalidUtf8)
    ));
}

#[test]
fn multibyte_text_can_resume_after_raw_continuations() {
    let mut protocol = Protocol::new(Role::Client, 1024, 1024);
    let mut wire = BytesMut::new();
    encode_frame(&mut wire, OpCode::Text, b"valid\xe7", false, None);
    assert!(protocol.process(&mut wire).unwrap().is_empty());
    encode_frame(
        &mut wire,
        OpCode::Continuation,
        b"\x95\x8c\xf0",
        false,
        None,
    );
    assert!(protocol.process_raw(&mut wire).unwrap().is_empty());
    encode_frame(&mut wire, OpCode::Continuation, b"\x9f\xa6\x80", true, None);
    let messages = protocol.process(&mut wire).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].as_bytes(), "valid界🦀".as_bytes());
}

#[test]
fn raw_processing_can_complete_invalid_text() {
    let mut protocol = Protocol::new(Role::Client, 1024, 1024);
    let mut wire = BytesMut::new();
    encode_frame(&mut wire, OpCode::Text, b"valid", false, None);
    assert!(protocol.process(&mut wire).unwrap().is_empty());
    encode_frame(&mut wire, OpCode::Continuation, b"\xff", true, None);
    let messages = protocol.process_raw(&mut wire).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].as_bytes(), b"valid\xff");
}

#[test]
fn raw_message_after_typed_message_does_not_inherit_validation() {
    let mut protocol = Protocol::new(Role::Client, 1024, 1024);
    let mut wire = BytesMut::new();
    encode_frame(&mut wire, OpCode::Text, b"previous message", false, None);
    encode_frame(&mut wire, OpCode::Continuation, b"!", true, None);
    assert_eq!(protocol.process(&mut wire).unwrap().len(), 1);
    encode_frame(&mut wire, OpCode::Text, b"\xff", false, None);
    assert!(protocol.process_raw(&mut wire).unwrap().is_empty());
    encode_frame(&mut wire, OpCode::Continuation, b"x", true, None);
    assert!(matches!(
        protocol.process(&mut wire),
        Err(Error::InvalidUtf8)
    ));
}

#[test]
fn consecutive_text_and_binary_fragments_preserve_payloads() {
    let mut wire = BytesMut::new();
    for (opcode, payload) in [
        (OpCode::Text, b"previous message".as_slice()),
        (OpCode::Binary, b"\xff\x80".as_slice()),
        (OpCode::Text, "界".as_bytes()),
        (OpCode::Text, b"".as_slice()),
    ] {
        encode_frame(
            &mut wire,
            opcode,
            &payload[..payload.len() / 2],
            false,
            None,
        );
        encode_frame(
            &mut wire,
            OpCode::Continuation,
            &payload[payload.len() / 2..],
            true,
            None,
        );
    }
    let messages = Protocol::new(Role::Client, 1024, 1024)
        .process(&mut wire)
        .unwrap();
    let payloads: Vec<_> = messages.iter().map(|message| message.as_bytes()).collect();
    assert_eq!(
        payloads,
        [
            b"previous message".as_slice(),
            b"\xff\x80".as_slice(),
            "界".as_bytes(),
            b"".as_slice(),
        ]
    );
}

#[test]
fn fragmented_text_respects_message_size_limit() {
    for limit in [3, 4] {
        let mut wire = BytesMut::new();
        encode_frame(&mut wire, OpCode::Text, b"\xf0\x9f", false, None);
        encode_frame(&mut wire, OpCode::Continuation, b"\xa6\x80", true, None);
        let result = Protocol::new(Role::Client, 1024, limit).process(&mut wire);
        if limit == 4 {
            assert_eq!(result.unwrap()[0].as_bytes(), "🦀".as_bytes());
        } else {
            assert!(matches!(result, Err(Error::MessageTooLarge)));
        }
    }
}
