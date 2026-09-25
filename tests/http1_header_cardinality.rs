use rstest::rstest;
use sockudo_ws::Error;
use sockudo_ws::handshake::{parse_request, parse_response};

const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
const ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

fn request_with(extra_headers: &str) -> Vec<u8> {
    format!(
        "GET /ws HTTP/1.1\r\n\
         Host: example.com\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {KEY}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         {extra_headers}\
         \r\n"
    )
    .into_bytes()
}

fn response_with(extra_headers: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {ACCEPT}\r\n\
         {extra_headers}\
         \r\n"
    )
    .into_bytes()
}

#[rstest]
#[case::key("sEc-WebSocket-Key", KEY, "duplicate Sec-WebSocket-Key")]
#[case::version("sEc-WebSocket-Version", "13", "duplicate Sec-WebSocket-Version")]
fn request_rejects_duplicate_singleton_field(
    #[case] name: &str,
    #[case] value: &str,
    #[case] expected: &str,
) {
    let header = format!("{name}: {value}\r\n");
    let error = parse_request(&request_with(&header)).unwrap_err();

    assert!(matches!(error, Error::HandshakeFailed(actual) if actual == expected));
}

#[rstest]
#[case::accept(
    "sEc-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n",
    "duplicate Sec-WebSocket-Accept"
)]
#[case::protocol(
    "Sec-WebSocket-Protocol: chat\r\nSec-WebSocket-Protocol: superchat\r\n",
    "duplicate Sec-WebSocket-Protocol"
)]
#[case::extensions(
    "Sec-WebSocket-Extensions: permessage-deflate\r\nSec-WebSocket-Extensions: x-test\r\n",
    "duplicate Sec-WebSocket-Extensions"
)]
fn response_rejects_duplicate_singleton_field(#[case] headers: &str, #[case] expected: &str) {
    let error = parse_response(&response_with(headers)).unwrap_err();

    assert!(matches!(error, Error::HandshakeFailed(actual) if actual == expected));
}

#[test]
fn request_accepts_repeated_protocol_and_extension_fields() {
    let headers = "Sec-WebSocket-Protocol: chat\r\n\
                   Sec-WebSocket-Protocol: superchat\r\n\
                   Sec-WebSocket-Extensions: permessage-deflate\r\n\
                   Sec-WebSocket-Extensions: x-test\r\n";

    assert!(parse_request(&request_with(headers)).unwrap().is_some());
}
