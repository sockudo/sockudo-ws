use bytes::BytesMut;
use sockudo_ws::frame::encode_frame;
use sockudo_ws::protocol::Protocol;
use sockudo_ws::{Error, Message, OpCode, Role};
#[cfg(feature = "permessage-deflate")]
use sockudo_ws::{
    deflate::DeflateConfig,
    protocol::{CompressedProtocol, CompressedReaderProtocol},
};

fn unmasked_frame(opcode: OpCode, payload: &[u8]) -> BytesMut {
    let mut buf = BytesMut::new();
    encode_frame(&mut buf, opcode, payload, true, None);
    buf
}

#[test]
fn single_message_limit_boundaries_cover_both_roles_and_payload_types() {
    for limit in [0, 4, 125, 126] {
        for size in [limit, limit + 1] {
            for opcode in [OpCode::Text, OpCode::Binary] {
                for role in [Role::Client, Role::Server] {
                    let mut wire = BytesMut::new();
                    let mask = (role == Role::Server).then_some([1, 2, 3, 4]);
                    encode_frame(&mut wire, opcode, &vec![b'x'; size], true, mask);
                    let mut results = vec![
                        Protocol::new(role, 1024, limit)
                            .process(&mut wire.clone())
                            .map(|messages| messages[0].as_bytes().len()),
                        Protocol::new(role, 1024, limit)
                            .process_raw(&mut wire.clone())
                            .map(|messages| messages[0].as_bytes().len()),
                    ];
                    #[cfg(feature = "permessage-deflate")]
                    {
                        let config = DeflateConfig::default();
                        let mut protocol = if role == Role::Server {
                            CompressedProtocol::server(1024, limit, config.clone())
                        } else {
                            CompressedProtocol::client(1024, limit, config.clone())
                        };
                        let mut reader = if role == Role::Server {
                            CompressedReaderProtocol::server(1024, limit, &config)
                        } else {
                            CompressedReaderProtocol::client(1024, limit, &config)
                        };
                        results.push(
                            protocol
                                .process(&mut wire.clone())
                                .map(|m| m[0].as_bytes().len()),
                        );
                        results.push(
                            reader
                                .process(&mut wire.clone())
                                .map(|m| m[0].as_bytes().len()),
                        );
                    }
                    for result in results.drain(..) {
                        if size == limit {
                            assert_eq!(result.unwrap(), size);
                        } else {
                            assert!(matches!(result, Err(Error::MessageTooLarge)));
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn single_frame_at_message_limit_is_accepted() {
    let mut protocol = Protocol::new(Role::Client, 1024, 4);
    let mut buf = unmasked_frame(OpCode::Text, b"test");

    let messages = protocol.process(&mut buf).unwrap();

    assert!(matches!(&messages[..], [Message::Text(text)] if text == "test"));
}

#[test]
fn single_frame_text_over_message_limit_is_rejected() {
    let mut protocol = Protocol::new(Role::Client, 1024, 4);
    let mut buf = unmasked_frame(OpCode::Text, b"large");

    let result = protocol.process(&mut buf);

    assert!(matches!(result, Err(Error::MessageTooLarge)));
}

#[test]
fn single_frame_binary_over_message_limit_is_rejected() {
    let mut protocol = Protocol::new(Role::Client, 1024, 4);
    let mut buf = unmasked_frame(OpCode::Binary, b"large");

    let result = protocol.process(&mut buf);

    assert!(matches!(result, Err(Error::MessageTooLarge)));
}

#[test]
fn raw_single_frame_text_over_message_limit_is_rejected() {
    let mut protocol = Protocol::new(Role::Client, 1024, 4);
    let mut buf = unmasked_frame(OpCode::Text, b"large");

    let result = protocol.process_raw(&mut buf);

    assert!(matches!(result, Err(Error::MessageTooLarge)));
}

#[test]
fn raw_single_frame_binary_over_message_limit_is_rejected() {
    let mut protocol = Protocol::new(Role::Client, 1024, 4);
    let mut buf = unmasked_frame(OpCode::Binary, b"large");

    let result = protocol.process_raw(&mut buf);

    assert!(matches!(result, Err(Error::MessageTooLarge)));
}

#[cfg(feature = "permessage-deflate")]
#[test]
fn compressed_protocol_single_frame_text_over_message_limit_is_rejected() {
    let mut protocol = CompressedProtocol::client(1024, 4, DeflateConfig::default());
    let mut buf = unmasked_frame(OpCode::Text, b"large");

    let result = protocol.process(&mut buf);

    assert!(matches!(result, Err(Error::MessageTooLarge)));
}

#[cfg(feature = "permessage-deflate")]
#[test]
fn compressed_protocol_single_frame_binary_over_message_limit_is_rejected() {
    let mut protocol = CompressedProtocol::client(1024, 4, DeflateConfig::default());
    let mut buf = unmasked_frame(OpCode::Binary, b"large");

    let result = protocol.process(&mut buf);

    assert!(matches!(result, Err(Error::MessageTooLarge)));
}

#[cfg(feature = "permessage-deflate")]
#[test]
fn compressed_reader_single_frame_text_over_message_limit_is_rejected() {
    let config = DeflateConfig::default();
    let mut protocol = CompressedReaderProtocol::client(1024, 4, &config);
    let mut buf = unmasked_frame(OpCode::Text, b"large");

    let result = protocol.process(&mut buf);

    assert!(matches!(result, Err(Error::MessageTooLarge)));
}

#[cfg(feature = "permessage-deflate")]
#[test]
fn compressed_reader_single_frame_binary_over_message_limit_is_rejected() {
    let config = DeflateConfig::default();
    let mut protocol = CompressedReaderProtocol::client(1024, 4, &config);
    let mut buf = unmasked_frame(OpCode::Binary, b"large");

    let result = protocol.process(&mut buf);

    assert!(matches!(result, Err(Error::MessageTooLarge)));
}
