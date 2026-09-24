use rstest::rstest;
use sockudo_ws::Error;
use sockudo_ws::handshake::{build_request_with_headers, parse_request};

const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

fn request_with(header: &str) -> Vec<u8> {
    format!(
        "GET /ws HTTP/1.1\r\n\
         Host: example.com\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {KEY}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         {header}\
         \r\n"
    )
    .into_bytes()
}

#[rstest]
#[case::single("Content-Length: 0\r\n")]
#[case::leading_zeros("Content-Length: 00\r\n")]
#[case::outer_ows("Content-Length:\t000\t\r\n")]
#[case::combined("Content-Length: 0, 0\r\n")]
#[case::combined_ows("Content-Length: 000,\t00 \r\n")]
#[case::repeated("Content-Length: 0\r\nContent-Length: 0\r\n")]
fn request_accepts_zero_content_length_values(#[case] header: &str) {
    assert!(parse_request(&request_with(header)).unwrap().is_some());
}

#[rstest]
#[case::nonzero("Content-Length: 1\r\n")]
#[case::combined_conflict("Content-Length: 0, 1\r\n")]
#[case::repeated_conflict("Content-Length: 0\r\nContent-Length: 1\r\n")]
#[case::empty("Content-Length:\r\n")]
#[case::trailing_comma("Content-Length: 0,\r\n")]
#[case::sign("Content-Length: +0\r\n")]
fn request_rejects_nonzero_or_invalid_content_length(#[case] header: &str) {
    let error = parse_request(&request_with(header)).unwrap_err();

    assert!(matches!(
        error,
        Error::InvalidHttp("WebSocket handshake must not contain a body")
    ));
}

#[rstest]
#[case::chunked("Transfer-Encoding: chunked\r\n")]
#[case::empty("Transfer-Encoding:\r\n")]
fn request_rejects_transfer_encoding(#[case] header: &str) {
    let error = parse_request(&request_with(header)).unwrap_err();

    assert!(matches!(
        error,
        Error::InvalidHttp("WebSocket handshake must not use Transfer-Encoding")
    ));
}

#[rstest]
#[case::content_length("Content-Length")]
#[case::transfer_encoding("Transfer-Encoding")]
fn request_builder_rejects_body_framing_headers(#[case] name: &str) {
    let headers = [(name.to_string(), "0".to_string())];
    let error = build_request_with_headers("example.com", "/ws", KEY, None, None, Some(&headers))
        .unwrap_err();

    assert!(matches!(
        error,
        Error::InvalidHttp("reserved handshake header")
    ));
}
