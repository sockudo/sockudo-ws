use bytes::BytesMut;
use sockudo_ws::error::CloseReason;
use sockudo_ws::protocol::Protocol;
use sockudo_ws::{Error, Message, RawMessage, Role};

// 1012–1014 are registered in the IANA WebSocket Close Code Number Registry.
const REGISTERED_SINCE_RFC_6455: [u16; 3] = [1012, 1013, 1014];
const NEVER_ON_THE_WIRE: [u16; 5] = [1004, 1005, 1006, 1015, 1016];

fn close_frame(code: u16) -> BytesMut {
    let [high, low] = code.to_be_bytes();
    BytesMut::from(&[0x88, 0x02, high, low][..])
}

fn received_code(messages: &[Message]) -> Option<u16> {
    match messages {
        [Message::Close(Some(reason))] => Some(reason.code),
        _ => None,
    }
}

#[test]
fn registered_close_codes_are_accepted() {
    for code in REGISTERED_SINCE_RFC_6455 {
        let mut protocol = Protocol::new(Role::Client, 1 << 20, 1 << 20);
        let messages = protocol.process(&mut close_frame(code)).unwrap();
        assert_eq!(received_code(&messages), Some(code));

        let mut protocol = Protocol::new(Role::Client, 1 << 20, 1 << 20);
        let messages = protocol.process_raw(&mut close_frame(code)).unwrap();
        assert!(matches!(
            messages.as_slice(),
            [RawMessage::Close(Some(reason))] if reason.code == code
        ));

        assert!(CloseReason::is_valid_code(code));
    }
}

#[test]
fn close_codes_reserved_from_the_wire_are_rejected() {
    for code in NEVER_ON_THE_WIRE {
        let mut protocol = Protocol::new(Role::Client, 1 << 20, 1 << 20);
        assert!(matches!(
            protocol.process(&mut close_frame(code)),
            Err(Error::InvalidCloseCode(rejected)) if rejected == code
        ));
        assert!(!CloseReason::is_valid_code(code));
    }
}

#[cfg(feature = "permessage-deflate")]
#[test]
fn compressed_protocols_accept_registered_close_codes() {
    use sockudo_ws::DeflateConfig;
    use sockudo_ws::protocol::{CompressedProtocol, CompressedReaderProtocol};

    for code in REGISTERED_SINCE_RFC_6455 {
        let mut protocol = CompressedProtocol::client(1 << 20, 1 << 20, DeflateConfig::default());
        let messages = protocol.process(&mut close_frame(code)).unwrap();
        assert_eq!(received_code(&messages), Some(code));

        let mut reader =
            CompressedReaderProtocol::client(1 << 20, 1 << 20, &DeflateConfig::default());
        let messages = reader.process(&mut close_frame(code)).unwrap();
        assert_eq!(received_code(&messages), Some(code));
    }
    for code in NEVER_ON_THE_WIRE {
        let mut reader =
            CompressedReaderProtocol::client(1 << 20, 1 << 20, &DeflateConfig::default());
        assert!(matches!(
            reader.process(&mut close_frame(code)),
            Err(Error::InvalidCloseCode(rejected)) if rejected == code
        ));
    }
}
