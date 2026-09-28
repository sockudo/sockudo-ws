use super::*;
use crate::frame::encode_frame;

#[test]
fn disabled_text_validation_matches_raw_at_every_read_boundary() {
    for role in [Role::Client, Role::Server] {
        let mask = (role == Role::Server).then_some([1, 2, 3, 4]);
        let mut wire = BytesMut::new();
        for (opcode, payload, fin) in [
            (OpCode::Text, &b"a\xffb"[..], true),
            (OpCode::Text, &b"\xe2\x82"[..], false),
            (OpCode::Ping, &b"ping"[..], true),
            (OpCode::Continuation, &b"\xacz\xff"[..], true),
            (OpCode::Binary, &b"\xff"[..], true),
        ] {
            encode_frame(&mut wire, opcode, payload, fin, mask);
        }
        for cut in 0..=wire.len() {
            let mut typed = Protocol::new(role, 1024, 1024).with_text_utf8_validation(false);
            let mut raw = Protocol::new(role, 1024, 1024);
            let mut typed_buf = BytesMut::new();
            let mut raw_buf = BytesMut::new();
            let mut actual = Vec::new();
            let mut expected = Vec::new();
            for chunk in [&wire[..cut], &wire[cut..]] {
                typed_buf.extend_from_slice(chunk);
                raw_buf.extend_from_slice(chunk);
                actual.extend(
                    typed
                        .process(&mut typed_buf)
                        .unwrap()
                        .into_iter()
                        .map(|m| (m.is_text(), m.into_bytes())),
                );
                expected.extend(
                    raw.process_raw(&mut raw_buf)
                        .unwrap()
                        .into_iter()
                        .map(|m| (matches!(m, RawMessage::Text(_)), m.into_bytes())),
                );
            }
            assert_eq!(actual, expected);
            assert_eq!(actual.len(), 4);
            assert_eq!(actual[0].1.as_ref(), b"a\xffb");
            assert_eq!(actual[2].1.as_ref(), b"\xe2\x82\xacz\xff");
            assert_eq!(typed.partial_checked, 0);
            assert_eq!(typed.fragment_validated_len, 0);
        }
    }
}

#[test]
fn disabled_validation_delivers_incomplete_codepoint_after_bytewise_reads() {
    let mut protocol = Protocol::new(Role::Client, 1024, 1024).with_text_utf8_validation(false);
    let mut input = BytesMut::new();
    let mut messages = Vec::new();
    for byte in b"\x81\x02x\xe2" {
        input.extend_from_slice(&[*byte]);
        messages.extend(protocol.process(&mut input).unwrap());
    }
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].as_bytes(), b"x\xe2");
}

#[test]
fn disabled_validation_preserves_accepted_messages_before_close_error() {
    let mut protocol = Protocol::new(Role::Client, 1024, 1024).with_text_utf8_validation(false);
    let mut wire = BytesMut::from(&b"\x81\x01\xff\x88\x03\x03\xe8\xff"[..]);
    let mut messages = Vec::new();
    assert!(matches!(
        protocol.process_into(&mut wire, &mut messages),
        Err(Error::InvalidUtf8)
    ));
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].as_bytes(), b"\xff");
    assert!(messages[0].as_text().is_none());
    assert!(messages.pop().unwrap().into_text().is_none());
}

#[test]
fn disabled_validation_still_enforces_fragment_order_and_message_limit() {
    for (wire, oversized) in [(&b"\x80\x01x"[..], false), (&b"\x81\x02\xff\xff"[..], true)] {
        let mut protocol = Protocol::new(Role::Client, 1024, 1).with_text_utf8_validation(false);
        let error = protocol.process(&mut BytesMut::from(wire)).unwrap_err();
        if oversized {
            assert!(matches!(error, Error::MessageTooLarge));
        } else {
            assert!(matches!(
                error,
                Error::Protocol("unexpected continuation frame")
            ));
        }
    }
}

#[cfg(feature = "permessage-deflate")]
#[test]
fn compressed_connection_accepts_unvalidated_fragments_across_split() {
    for compressed in [false, true] {
        let payload = b"\xffbad text\xe2".repeat(32);
        let encoded = if compressed {
            crate::deflate::DeflateEncoder::new(crate::deflate::MAX_WINDOW_BITS, true, 6, 0)
                .compress(&payload)
                .unwrap()
                .unwrap()
        } else {
            Bytes::copy_from_slice(&payload)
        };
        let cut = encoded.len() / 2;
        let mut first = BytesMut::new();
        crate::frame::encode_frame_with_rsv(
            &mut first,
            OpCode::Text,
            &encoded[..cut],
            false,
            None,
            compressed,
        );
        let mut last = BytesMut::new();
        encode_frame(&mut last, OpCode::Continuation, &encoded[cut..], true, None);
        let mut protocol = CompressedProtocol::client(1024, 1024, DeflateConfig::default())
            .with_text_utf8_validation(false);
        assert!(protocol.process(&mut first.clone()).unwrap().is_empty());
        let (mut reader, _) = protocol.split(1024, 1024);
        let messages = reader.process(&mut last.clone()).unwrap();
        assert_eq!(messages[0].as_bytes(), payload);
        let mut unified = CompressedProtocol::client(1024, 1024, DeflateConfig::default())
            .with_text_utf8_validation(false);
        assert!(unified.process(&mut first).unwrap().is_empty());
        assert_eq!(unified.process(&mut last).unwrap()[0].as_bytes(), payload);
    }
}

#[cfg(any(feature = "tokio-runtime", feature = "compio-runtime"))]
#[test]
fn ordinary_split_preserves_unvalidated_partial_text() {
    let mut protocol = Protocol::new(Role::Client, 1024, 1024).with_text_utf8_validation(false);
    let mut wire = BytesMut::from(&b"\x01\x01\xff\x80\x02\xe2"[..]);
    assert!(protocol.process(&mut wire).unwrap().is_empty());
    let (mut reader, _) = protocol.split(1024, 1024);
    wire.extend_from_slice(b"\xff");
    let messages = reader.process(&mut wire).unwrap();
    assert_eq!(messages[0].as_bytes(), b"\xff\xe2\xff");
}

#[cfg(feature = "permessage-deflate")]
#[rstest::rstest]
#[case::enabled_unified(true, false)]
#[case::enabled_split(true, true)]
#[case::disabled_unified(false, false)]
#[case::disabled_split(false, true)]
fn compressed_complete_text_obeys_policy(#[case] enabled: bool, #[case] split: bool) {
    let payload = b"\xffbad text".repeat(32);
    let compressed =
        crate::deflate::DeflateEncoder::new(crate::deflate::MAX_WINDOW_BITS, true, 6, 0)
            .compress(&payload)
            .unwrap()
            .unwrap();
    let mut wire = BytesMut::new();
    crate::frame::encode_frame_with_rsv(&mut wire, OpCode::Text, &compressed, true, None, true);
    let mut protocol = CompressedProtocol::client(1024, 1024, DeflateConfig::default())
        .with_text_utf8_validation(enabled);
    let result = if split {
        protocol.split(1024, 1024).0.process(&mut wire)
    } else {
        protocol.process(&mut wire)
    };
    if enabled {
        assert!(matches!(result, Err(Error::InvalidUtf8)));
    } else {
        assert_eq!(result.unwrap()[0].as_bytes(), payload);
    }
}

#[cfg(feature = "permessage-deflate")]
#[rstest::rstest]
#[case::uncompressed_unified(false, false)]
#[case::uncompressed_split(false, true)]
#[case::compressed_unified(true, false)]
#[case::compressed_split(true, true)]
fn compressed_connection_rejects_invalid_final_text_fragment_by_default(
    #[case] compressed: bool,
    #[case] split: bool,
) {
    // Keep the first uncompressed fragment valid so rejection must occur at
    // message completion, rather than during the initial incremental check.
    let mut payload = b"valid prefix ".repeat(16);
    payload.push(0xff);
    let encoded = if compressed {
        crate::deflate::DeflateEncoder::new(crate::deflate::MAX_WINDOW_BITS, true, 6, 0)
            .compress(&payload)
            .unwrap()
            .unwrap()
    } else {
        Bytes::copy_from_slice(&payload)
    };
    let cut = encoded.len() / 2;
    let mut first = BytesMut::new();
    crate::frame::encode_frame_with_rsv(
        &mut first,
        OpCode::Text,
        &encoded[..cut],
        false,
        None,
        compressed,
    );
    let mut last = BytesMut::new();
    encode_frame(&mut last, OpCode::Continuation, &encoded[cut..], true, None);
    let mut protocol = CompressedProtocol::client(1024, 1024, DeflateConfig::default());
    assert!(protocol.process(&mut first).unwrap().is_empty());
    let result = if split {
        protocol.split(1024, 1024).0.process(&mut last)
    } else {
        protocol.process(&mut last)
    };
    assert!(matches!(result, Err(Error::InvalidUtf8)));
}
